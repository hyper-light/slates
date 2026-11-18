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

impl slates_wire::Wire for HostId {
  /// Format: a host identity is one unsigned 64-bit word.
  const SCHEMA: &'static str = "struct HostId{value:u64}";
  /// Format: the named wrapper's schema includes its word's schema, as a derived struct does.
  const SCHEMA_HASH: u64 = slates_wire::schema::mix(
    slates_wire::schema::fnv64(Self::SCHEMA),
    &[<u64 as slates_wire::Wire>::SCHEMA_HASH],
  );

  fn encode(&self, out: &mut Vec<u8>) {
    self.0.encode(out);
  }

  fn decode(input: &mut &[u8]) -> Result<Self, slates_wire::WireError> {
    u64::decode(input).map(HostId)
  }
}

/// A region in the fleet (§4.8, D-14 — "a root group across regions holds region membership"). A region is
/// one regional configuration group's domain; the **root group** agrees on which regions exist, where a
/// moved volume is homed, and which region a lost region is promoted to. One region on a laptop (the sole
/// region, id `0`), so the root group is the degenerate self-leading group R8 requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionId(pub u64);

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
    let (high, _) = self.halves();
    HostId(u64::from_be_bytes(high))
  }

  /// The unique per-creator suffix in the low 8 bytes.
  pub fn local(&self) -> u64 {
    let (_, low) = self.halves();
    u64::from_be_bytes(low)
  }

  /// The high and low eight bytes.
  fn halves(&self) -> ([u8; size_of::<u64>()], [u8; size_of::<u64>()]) {
    let (high, low) = self.0.split_at(size_of::<u64>());
    (
      high.try_into().unwrap_or_default(),
      low.try_into().unwrap_or_default(),
    )
  }

  /// The catalog register of this object (§4.8 "catalog entries are registers the owner writes under that
  /// epoch"): the same id with the register-class bit set. It is placed, owned and taken over exactly as
  /// its object is ([`ObjectId::placement_key`]), so a volume's catalog and its head always share their
  /// holders and their successor.
  pub const fn catalog(self) -> ObjectId {
    let mut bytes = self.0;
    bytes[CLASS_BYTE] |= CATALOG_CLASS;
    ObjectId(bytes)
  }

  /// Whether this id names a catalog register ([`ObjectId::catalog`]).
  pub const fn is_catalog(&self) -> bool {
    self.0[CLASS_BYTE] & CATALOG_CLASS != 0
  }

  /// The object whose placement and takeover this id follows: the id itself with its register-class bit
  /// cleared, so every register class of one object ranks the same holders and the same successor.
  pub const fn placement_key(&self) -> ObjectId {
    let mut bytes = self.0;
    bytes[CLASS_BYTE] &= !CATALOG_CLASS;
    ObjectId(bytes)
  }
}

/// Format: the byte of an object id carrying its register-class bit — the first byte of the per-creator
/// counter (bytes 10–15 of a volume id, after the creator and the partition).
const CLASS_BYTE: usize = 10;
/// Format: the register-class bit — the counter's top bit — set for a catalog register.
const CATALOG_CLASS: u8 = 0x80;
/// The largest per-creator counter an object id may carry: the counter's 48 bits less the class bit, so
/// no object's own id ever reads as a catalog register. An id minter refuses past it.
pub const MAX_OBJECT_COUNTER: u64 = (1u64 << 47) - 1;

/// The width of an encoded object id (16 bytes) — its place in every register wire layout.
pub const OBJECT_BYTES: usize = size_of::<ObjectId>();

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

  /// Whether `acked` acknowledgements from a joint write's cohort of `cohort` hosts satisfy that cohort (§4.8 "joint
  /// writes"): `f + 1` of it, or every member when it has fewer than `f + 1`, and at least one. A cohort smaller than
  /// the quorum is the one a bootstrap settles (the first node alone) while its neighbourhood grows to the region; it
  /// could never give `f + 1`, so a record joined with it never committed and the change it waited on never settled
  /// (the bug record of 2026-11-06). Every member of it is the most a takeover recovering through it could meet.
  pub fn committed_within(self, acked: usize, cohort: usize) -> bool {
    acked >= self.commit().min(cohort).max(1)
  }

  /// Derived: the promises a takeover's phase one needs from a recovery cohort of `cohort` hosts before it
  /// adopts, so that its answers meet every record that could have committed there. A commit is `f + 1`
  /// acknowledgements of the cohort, so `cohort − f` promises meet every such set — `q1 + q2 > n`, the
  /// intersection Flexible Paxos proves sufficient [A: Howard, Malkhi, Spiegelman, "Flexible Paxos:
  /// Quorum intersection revisited", OPODIS 2016] — and one fewer can miss one. At the candidate floor
  /// (`cohort = 2f + 1`) that is `f + 1`. A cohort a forming region fixed with fewer hosts needs fewer: a
  /// commit there is held by more of its hosts, so fewer answers meet it (two hosts at `f = 1` commit only with
  /// both, so either one's promise suffices); demanding `f + 1` there declared a record lost while its cohort's
  /// survivor held it. A cohort of `f` or fewer hosts commits a joint write only with every one of its hosts
  /// ([`Quorum::committed_within`]), so one promise meets it, and a round adopts only what a promise reports, so it
  /// needs one.
  pub fn recovery(self, cohort: usize) -> usize {
    cohort
      .saturating_sub(usize::try_from(self.f).unwrap_or(usize::MAX))
      .max(1)
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
  /// The candidate holders the record is sent to: its `2f + 1` candidates, the owner among them — and, while
  /// the owner's neighbourhood change is in flight, the union of its settled and its current cohorts.
  pub candidates: Vec<HostId>,
  /// The candidates that acknowledged, recorded in the head so a reader learns the copies.
  pub acked: Vec<HostId>,
  /// The mirror candidates that acknowledged, when the record was shipped to the mirror.
  pub mirror_acked: Option<Vec<HostId>>,
  /// While the owner's neighbourhood change is in flight (§4.8 "the owner writes to a quorum of the old
  /// candidates and a quorum of the new ones (joint writes)"), the two cohorts the record must reach `f + 1`
  /// of **each** — the settled one first, then the current one. Empty outside a change, and for an object
  /// whose cohort the change left as it was: `candidates` is then the one cohort.
  pub joint: Vec<Vec<HostId>>,
}

impl Placement {
  /// The placement of an object at the moment its owner holds it and nothing else has yet: at
  /// `f = 0` this already commits (the owner is `f + 1`); at `f > 0` it is `Local` until
  /// holders acknowledge.
  pub fn local(
    owner: HostId,
    quorum: Quorum,
    neighbourhood: &[HostId],
    domains: &std::collections::BTreeMap<HostId, DomainId>,
    object: ObjectId,
  ) -> Placement {
    let candidates = candidates_for(owner, neighbourhood, domains, object, quorum);
    let acked = if quorum.committed(1) {
      vec![owner]
    } else {
      Vec::new()
    };
    Placement {
      candidates,
      acked,
      mirror_acked: None,
      joint: Vec::new(),
    }
  }

  /// Whether the region commit holds (`f + 1` regional acknowledgements) — of **each** cohort while a change
  /// is in flight ([`joint`](Placement::joint)). Counts each **candidate** once: a duplicate acknowledgement
  /// from one holder, or an acknowledgement bearing a host id that is not a candidate for this object (or not
  /// in the cohort being counted), cannot manufacture a quorum (a two-count `f = 1` commit must be two
  /// *distinct* candidates). The vector length alone is not the count.
  pub fn placed(&self, quorum: Quorum) -> bool {
    self.placed_with(&self.acked, quorum)
  }

  /// Whether `acked` would place a record under this placement's candidates and cohorts — [`placed`] for an
  /// acknowledging set gathered apart from it, as a dispatch does while it collects replies.
  ///
  /// [`placed`]: Placement::placed
  pub fn placed_with(&self, acked: &[HostId], quorum: Quorum) -> bool {
    if self.joint.is_empty() {
      return quorum.committed(self.distinct_candidate_acks(acked));
    }
    self
      .joint
      .iter()
      .all(|cohort| quorum.committed_within(distinct_acks_within(cohort, acked), cohort.len()))
  }

  /// The single-cohort placement of `candidates` that nothing has acknowledged yet — the shape a dispatch to
  /// one candidate set (content, a laptop, a test) commits against.
  pub fn of(candidates: &[HostId]) -> Placement {
    Placement {
      candidates: candidates.to_vec(),
      acked: Vec::new(),
      mirror_acked: None,
      joint: Vec::new(),
    }
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
    distinct_acks_within(&self.candidates, acks)
  }
}

/// The number of distinct hosts of `cohort` in `acks`: dedups the host ids and drops any outside `cohort`.
fn distinct_acks_within(cohort: &[HostId], acks: &[HostId]) -> usize {
  acks
    .iter()
    .filter(|host| cohort.contains(host))
    .collect::<std::collections::BTreeSet<&HostId>>()
    .len()
}

/// The candidate holders for `object`: the owner plus the co-holders of the one **copyset** the object
/// rendezvous-maps onto within the owner's neighbourhood (`owner_copysets` then `copyset_for`), `2f + 1`
/// total when the neighbourhood is domain-rich enough. Rendezvous runs over the *copysets*, not the raw
/// hosts, so the number of distinct copysets stays linear in the scatter width, not the `Θ(S^{2f})` a
/// per-object rendezvous over the hosts would make (research §3.1; [A: Cidon et al., ATC 2013]).
/// Deterministic — every host computes the same set from the object id, the neighbourhood and the domain
/// map, with no directory (§4.8 "Placement"). `domains` gives each host's failure domain (a host absent
/// from it is its own, `host.0`), so no copyset repeats a domain. At `f = 0` this is the owner alone; at
/// the candidate floor `S = 2f+1` the neighbourhood is a single copyset, so this is the owner plus its `2f`
/// co-holders — the same set the former per-object rendezvous produced, leaving the floor unchanged.
pub fn candidates_for(
  owner: HostId,
  neighbourhood: &[HostId],
  domains: &std::collections::BTreeMap<HostId, DomainId>,
  object: ObjectId,
  quorum: Quorum,
) -> Vec<HostId> {
  // Pair each neighbourhood host with its failure domain — the deployment's, or `host.0` (its own) when
  // undeclared (unique-per-host, the safe laptop/flat-cluster default) — so the copyset construction keeps
  // every copyset across distinct domains. A richer domain source is thus a change to the map alone.
  let with_domains: Vec<(HostId, DomainId)> = neighbourhood
    .iter()
    .map(|host| (*host, domains.get(host).copied().unwrap_or(host.0)))
    .collect();
  let copysets = owner_copysets(owner, &with_domains, quorum);
  copyset_for(object, &copysets).unwrap_or_else(|| vec![owner])
}

/// The **bounded neighbourhood** the configuration assigns `owner` from the alive set (§4.8 "Placement";
/// D-14 "a fixed set of hosts across failure domains whose size — the scatter width — bounds the copyset
/// count"): the owner plus the top `scatter - 1` other alive hosts by rendezvous weight *keyed on the
/// owner*, so the choice is deterministic (every node computes the same neighbourhood for `owner`),
/// owner-specific (a different owner scatters over a different set), and stable — a membership change moves
/// only the hosts whose rendezvous rank crossed the cut, the "add before remove" the design calls for
/// (§4.8). The caller (the regional configuration council) passes the derived scatter width, defaulting to the candidate floor
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
  // Every register class of one object hashes as that object, so its classes share holders and successor.
  let key = object.placement_key();
  for byte in host.to_le_bytes().iter().chain(key.0.iter()) {
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
  rendezvous_ranked(hosts, object).first().copied()
}

/// `hosts` in rendezvous order for `object`: the highest weight first, the lowest id breaking a tie (the order
/// [`rendezvous_first`] takes the head of). A mirror region's cohort is its prefix (`crate::mirror::mirror_cohort`).
pub fn rendezvous_ranked(hosts: &[HostId], object: ObjectId) -> Vec<HostId> {
  let mut ranked: Vec<(u64, HostId)> = hosts
    .iter()
    .map(|host| (rendezvous_weight(host.0, &object), *host))
    .collect();
  ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  ranked.into_iter().map(|(_, host)| host).collect()
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
  // `copies >= 2` here, so neither the subtraction nor the division can fail.
  let permutations = scatter.div_ceil(copies.saturating_sub(1).max(1));
  permutations
    .saturating_mul(hosts)
    .checked_div(copies)
    .unwrap_or(0)
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
  hosts.saturating_mul(binomial(scatter, copies.saturating_sub(1)))
}

/// `C(n, k)` computed iteratively without overflow for the small `k` (`copies − 1`) placement uses;
/// saturating so an out-of-range input cannot panic. `C(n, k) = 0` for `k > n`.
fn binomial(n: u64, k: u64) -> u64 {
  if k > n {
    return 0;
  }
  let k = k.min(n.saturating_sub(k));
  let mut result: u64 = 1;
  for i in 0..k {
    // `i < k <= n`, so `n - i >= 1` and the divisor `i + 1 >= 1`.
    result = result
      .saturating_mul(n.saturating_sub(i))
      .checked_div(i.saturating_add(1))
      .unwrap_or(0);
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
  let candidate_floor = f.saturating_mul(2).saturating_add(1);
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
/// nodes fail (no full copyset can be inside the failed set). One copy has the ordinary
/// single-host loss probability; lack of redundancy never implies lack of risk.
pub fn coincident_loss_probability(copysets: u64, hosts: u64, failed: u64, copies: u64) -> f64 {
  if copies == 0 || failed < copies || hosts < copies {
    return 0.0;
  }
  // C(failed, copies) / C(hosts, copies) = ∏_{i=0}^{copies-1} (failed - i) / (hosts - i).
  #[allow(clippy::cast_precision_loss)]
  let ratio: f64 = (0..copies)
    .map(|i| failed.saturating_sub(i) as f64 / hosts.saturating_sub(i).max(1) as f64)
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
    let mut out = Vec::with_capacity(RECORD_PREFIX_BYTES.saturating_add(self.value.len()));
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

/// Format: the one-byte tag that marks a record reply as a carried refusal rather than an [`Ack`] (§4.8 the
/// piggyback rule). A `ConfigurationStale` reply is `[STALE_REFUSAL_TAG][version: u64 LE]` — nine bytes,
/// which [`Ack::decode`] rejects on length ([`ACK_BYTES`] is 72), so the two reply shapes never collide.
const STALE_REFUSAL_TAG: u8 = 1;

/// A record refusal a *sender* acts on, decoded from a holder's reply that is not an [`Ack`]. The only
/// refusal carried back on the record wire is [`ConfigurationStale`](RegisterError::ConfigurationStale): a
/// holder on a newer configuration names its current `version` so the stale sender refreshes and retries
/// (§4.8 "refreshes consume the operation's bounded retry budget"). Every other refusal carries nothing back
/// (an empty reply the sender counts as no acknowledgement) — see [`encode_refusal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
  /// The holder is on a newer configuration; `version` is its current one, the version the sender refreshes
  /// toward.
  ConfigurationStale {
    /// The holder's current (newer) configuration version.
    version: u64,
  },
}

/// The wire form of a record refusal the sender acts on (§4.8 the piggyback rule). A
/// [`ConfigurationStale`](RegisterError::ConfigurationStale) rides back so a sender on an older configuration
/// refreshes and retries within its budget. Every other refusal encodes to an **empty** reply — the sender
/// counts no acknowledgement and retries as before: an [`Unauthorized`](RegisterError::Unauthorized) or
/// [`ConflictingPosition`](RegisterError::ConflictingPosition) is not a staleness a refresh resolves, and a
/// [`StaleEpoch`](RegisterError::StaleEpoch) on the record path is shadowed by this gate — a superseded owner
/// is at an older generation, so it is refused `ConfigurationStale` before the fence is reached, and the
/// refresh that refusal drives installs the configuration that reassigns the object away (the design's "the
/// latter refusal ends the sender's authority", realized through the version refresh).
pub fn encode_refusal(error: &RegisterError) -> Vec<u8> {
  match error {
    RegisterError::ConfigurationStale { version } => {
      let mut out = Vec::with_capacity(size_of::<u64>().saturating_add(1));
      out.push(STALE_REFUSAL_TAG);
      out.extend_from_slice(&version.to_le_bytes());
      out
    }
    _ => Vec::new(),
  }
}

/// Decodes a holder's record reply that did not decode as an [`Ack`] into the [`Refusal`] the sender acts on,
/// or `None` if it carries nothing actionable (an empty reply, or bytes that are neither an ack nor a tagged
/// refusal — including hostile input, which is simply ignored). The length and tag disambiguate: a carried
/// `ConfigurationStale` is exactly `1 + 8` bytes led by [`STALE_REFUSAL_TAG`], which no [`Ack`] (72 bytes)
/// can match.
pub fn decode_refusal(bytes: &[u8]) -> Option<Refusal> {
  let (&tag, version) = bytes.split_first()?;
  if tag != STALE_REFUSAL_TAG {
    return None;
  }
  let version = u64::from_le_bytes(version.try_into().ok()?);
  Some(Refusal::ConfigurationStale { version })
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
        let fixed = size_of::<u64>()
          .saturating_mul(2)
          .saturating_add(size_of::<u32>());
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

/// A successor's phase-one request for one page of a retired host's objects (§4.8 "Promotion and takeover":
/// "each new owner runs phase one in one batched round per register class across the neighbourhood: every
/// holder raises its fence for that host to the new epoch and reports the highest record it holds for each
/// object"): the retired host, the asking successor, the epoch it will write the objects under, the
/// configuration generation the round runs under, and the object after which this page starts (`None` for the
/// first). One round per retired host replaces a round per object: a holder answers for all of that host's
/// objects whose successor is the asker, and a holder that holds nothing answers too.
///
/// A request may instead name objects (A-251 (b)): the successor asks a holder whose pages are complete about each
/// object it learned that the holder did not list, and the holder answers each alone, from its own records, with no
/// routing in the way. A request that names objects asks no page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPrepare {
  /// The retired host whose objects are taken over.
  pub departed: HostId,
  /// The successor asking.
  pub owner: HostId,
  /// The epoch the successor writes the adopted objects under — the ballot the holder promises.
  pub epoch: HostEpoch,
  /// The configuration generation the round runs under; a holder on another refuses.
  pub generation: u64,
  /// The object after which this page starts, `None` for the first page.
  pub after: Option<ObjectId>,
  /// The objects asked about by name, each answered alone; empty for a page request. Appended to the wire only when
  /// not empty, so a page request's bytes are as before A-251.
  pub named: Vec<ObjectId>,
}

/// Format: an absent optional field on the takeover wire.
const FIELD_ABSENT: u8 = 0;
/// Format: a present optional field on the takeover wire; its value follows.
const FIELD_PRESENT: u8 = 1;

/// Format: a page request's widest bytes ([`HostPrepare`]): departed, owner, epoch, generation (u64 LE each), then the
/// page start's presence byte and object. A request naming objects appends their count (u32 LE) and the objects.
pub const HOST_PREPARE_BYTES: usize = 4 * size_of::<u64>() + 1 + OBJECT_BYTES;

/// Format: the bytes an appended list of objects takes before its objects: its count (u32 LE).
pub const OBJECT_LIST_HEADER_BYTES: usize = size_of::<u32>();

impl HostPrepare {
  /// The canonical bytes: departed, owner, epoch, generation (each u64 LE), then a presence byte and, when
  /// present, the page's starting object.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(HOST_PREPARE_BYTES);
    out.extend_from_slice(&self.departed.0.to_le_bytes());
    out.extend_from_slice(&self.owner.0.to_le_bytes());
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    put_optional_object(&mut out, self.after);
    put_object_list(&mut out, &self.named);
    out
  }

  /// Reconstructs a request from its bytes, or [`RegisterError::MalformedRecord`] for anything short, long, or
  /// carrying an unknown presence byte (a message that crossed the network — hostile input).
  pub fn decode(bytes: &[u8]) -> Result<HostPrepare, RegisterError> {
    let mut reader = WireReader::new(bytes);
    let departed = HostId(reader.word()?);
    let owner = HostId(reader.word()?);
    let epoch = HostEpoch(reader.word()?);
    let generation = reader.word()?;
    let after = reader.optional_object()?;
    let named = reader.object_list()?;
    reader.finish()?;
    Ok(HostPrepare {
      departed,
      owner,
      epoch,
      generation,
      after,
      named,
    })
  }
}

/// A holder's answer to one page of a [`HostPrepare`]: the objects of the retired host it holds whose successor
/// is the asker, each with the highest record it holds — its fence raised to the epoch and the asker's
/// authority installed before this answer was formed — where the next page starts if the holder has more, and
/// the highest fence it found above the epoch on an object it therefore could not promise, so the asker retries
/// above it. A page promises what it lists, and nothing of what it does not (A-251): a holder's routing decides what
/// its pages list, and it can leave out a copy it holds. An object the holder did not list is promised only by name:
/// the asker names it ([`HostPrepare::named`]) and the holder reports its record or, in `nothing`, that it holds none.
/// That empty promise binds as a listed one does: the retired host is no member, so no holder accepts its records, and
/// the successor is the only member the configuration names for the object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPromise {
  /// The answering holder.
  pub holder: HostId,
  /// The retired host (echoes the request).
  pub departed: HostId,
  /// The epoch promised (echoes the request).
  pub epoch: HostEpoch,
  /// The generation the holder answered under (echoes the request).
  pub generation: u64,
  /// The objects on this page, each with the highest record the holder holds for it.
  pub entries: Vec<(ObjectId, Accepted)>,
  /// The object after which the next page starts, `None` when this page completes the answer.
  pub next: Option<ObjectId>,
  /// The highest fence above the epoch on an object this holder could not promise, if any.
  pub fenced: Option<HostEpoch>,
  /// How many objects this page's range holds a copy of that this holder could not promise for any other reason: such
  /// a page is no answer for what it leaves out, so the asker does not count it as complete. Listing nothing means
  /// holding nothing only when this is zero.
  pub unpromised: u32,
  /// Of the objects a request named (A-251 (b)), those this holder holds nothing of: its promise of each, alone. A
  /// named object it holds is among `entries`; one it could not answer is in neither. Appended to the wire only when
  /// not empty.
  pub nothing: Vec<ObjectId>,
}

/// Format: the fixed width of one [`HostPromise`] entry before its value: the object (16 bytes), the record's
/// sequence and epoch (u64 LE each), and the value's length (u32 LE).
pub const HOST_PROMISE_ENTRY_BYTES: usize = OBJECT_BYTES + 2 * size_of::<u64>() + size_of::<u32>();

/// Format: the widest [`HostPromise`] header: holder, departed, epoch, generation (u64 LE each), the next
/// page's presence byte and object, the fence's presence byte and epoch, the unpromised count and the entry count
/// (u32 LE each).
pub const HOST_PROMISE_HEADER_BYTES: usize =
  4 * size_of::<u64>() + 1 + OBJECT_BYTES + 1 + size_of::<u64>() + 2 * size_of::<u32>();

impl HostPromise {
  /// Whether this answers `prepare`: the same retired host, epoch and generation, so an answer to another
  /// round cannot be counted for this one.
  pub fn binds(&self, prepare: &HostPrepare) -> bool {
    self.departed == prepare.departed
      && self.epoch == prepare.epoch
      && self.generation == prepare.generation
  }

  /// The bytes one entry holding `accepted` adds to an answer — what a holder paging its answer counts against
  /// the frame it must fit in.
  pub fn entry_bytes(accepted: &Accepted) -> usize {
    HOST_PROMISE_ENTRY_BYTES.saturating_add(accepted.value.len())
  }

  /// The canonical bytes: the header words, the next page's and the fence's optional fields, the entry count,
  /// then each entry (object, sequence, epoch, value length, value).
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(HOST_PROMISE_HEADER_BYTES);
    out.extend_from_slice(&self.holder.0.to_le_bytes());
    out.extend_from_slice(&self.departed.0.to_le_bytes());
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    put_optional_object(&mut out, self.next);
    match self.fenced {
      None => out.push(FIELD_ABSENT),
      Some(fence) => {
        out.push(FIELD_PRESENT);
        out.extend_from_slice(&fence.0.to_le_bytes());
      }
    }
    out.extend_from_slice(&self.unpromised.to_le_bytes());
    let count = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for (object, accepted) in &self.entries {
      out.extend_from_slice(&object.0);
      out.extend_from_slice(&accepted.sequence.to_le_bytes());
      out.extend_from_slice(&accepted.epoch.0.to_le_bytes());
      let value_len = u32::try_from(accepted.value.len()).unwrap_or(u32::MAX);
      out.extend_from_slice(&value_len.to_le_bytes());
      out.extend_from_slice(&accepted.value);
    }
    put_object_list(&mut out, &self.nothing);
    out
  }

  /// Reconstructs an answer from its bytes, checking every length against what remains before reading or
  /// allocating — an entry count beyond what the bytes could hold, a value length past the end, an unknown
  /// presence byte, or trailing bytes are [`RegisterError::MalformedRecord`], never a panic or an over-read.
  pub fn decode(bytes: &[u8]) -> Result<HostPromise, RegisterError> {
    let mut reader = WireReader::new(bytes);
    let holder = HostId(reader.word()?);
    let departed = HostId(reader.word()?);
    let epoch = HostEpoch(reader.word()?);
    let generation = reader.word()?;
    let next = reader.optional_object()?;
    let fenced = match reader.byte()? {
      FIELD_ABSENT => None,
      FIELD_PRESENT => Some(HostEpoch(reader.word()?)),
      _ => return Err(RegisterError::MalformedRecord),
    };
    let unpromised = reader.half()?;
    let count = usize::try_from(reader.half()?).map_err(|_| RegisterError::MalformedRecord)?;
    if count > reader.remaining() / HOST_PROMISE_ENTRY_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
      let object = reader.object()?;
      let sequence = reader.word()?;
      let entry_epoch = HostEpoch(reader.word()?);
      let value_len =
        usize::try_from(reader.half()?).map_err(|_| RegisterError::MalformedRecord)?;
      let value = reader.take(value_len)?.to_vec();
      entries.push((
        object,
        Accepted {
          sequence,
          epoch: entry_epoch,
          value,
        },
      ));
    }
    let nothing = reader.object_list()?;
    reader.finish()?;
    Ok(HostPromise {
      holder,
      departed,
      epoch,
      generation,
      entries,
      next,
      fenced,
      unpromised,
      nothing,
    })
  }
}

/// Appends an optional object: a presence byte, then the object when present.
fn put_optional_object(out: &mut Vec<u8>, object: Option<ObjectId>) {
  match object {
    None => out.push(FIELD_ABSENT),
    Some(object) => {
      out.push(FIELD_PRESENT);
      out.extend_from_slice(&object.0);
    }
  }
}

/// Appends a list of objects when it is not empty: its count, then the objects. An empty list writes nothing, so a
/// message without one keeps its earlier bytes.
fn put_object_list(out: &mut Vec<u8>, objects: &[ObjectId]) {
  if objects.is_empty() {
    return;
  }
  out.extend_from_slice(
    &u32::try_from(objects.len())
      .unwrap_or(u32::MAX)
      .to_le_bytes(),
  );
  for object in objects {
    out.extend_from_slice(&object.0);
  }
}

/// A bounds-checked reader over bytes that crossed the network: every read checks what remains first, so a
/// short or lying message is [`RegisterError::MalformedRecord`], never a panic or an over-read.
struct WireReader<'a> {
  rest: &'a [u8],
}

impl<'a> WireReader<'a> {
  /// A reader over `bytes`.
  fn new(bytes: &'a [u8]) -> WireReader<'a> {
    WireReader { rest: bytes }
  }

  /// The next `len` bytes.
  fn take(&mut self, len: usize) -> Result<&'a [u8], RegisterError> {
    let (head, rest) = self
      .rest
      .split_at_checked(len)
      .ok_or(RegisterError::MalformedRecord)?;
    self.rest = rest;
    Ok(head)
  }

  /// The next little-endian u64.
  fn word(&mut self) -> Result<u64, RegisterError> {
    let bytes = self.take(size_of::<u64>())?;
    Ok(u64::from_le_bytes(
      bytes
        .try_into()
        .map_err(|_| RegisterError::MalformedRecord)?,
    ))
  }

  /// The next little-endian u32.
  fn half(&mut self) -> Result<u32, RegisterError> {
    let bytes = self.take(size_of::<u32>())?;
    Ok(u32::from_le_bytes(
      bytes
        .try_into()
        .map_err(|_| RegisterError::MalformedRecord)?,
    ))
  }

  /// The next byte.
  fn byte(&mut self) -> Result<u8, RegisterError> {
    let (&byte, rest) = self
      .rest
      .split_first()
      .ok_or(RegisterError::MalformedRecord)?;
    self.rest = rest;
    Ok(byte)
  }

  /// The next object id.
  fn object(&mut self) -> Result<ObjectId, RegisterError> {
    let bytes = self.take(OBJECT_BYTES)?;
    Ok(ObjectId(
      bytes
        .try_into()
        .map_err(|_| RegisterError::MalformedRecord)?,
    ))
  }

  /// The next optional object: a presence byte, then the object when present.
  fn optional_object(&mut self) -> Result<Option<ObjectId>, RegisterError> {
    match self.byte()? {
      FIELD_ABSENT => Ok(None),
      FIELD_PRESENT => Ok(Some(self.object()?)),
      _ => Err(RegisterError::MalformedRecord),
    }
  }

  /// An appended list of objects ([`put_object_list`]): empty when no bytes are left; otherwise a count of at least one
  /// that the bytes left can hold, then the objects. A count of zero is refused, so each list has one encoding.
  fn object_list(&mut self) -> Result<Vec<ObjectId>, RegisterError> {
    if self.rest.is_empty() {
      return Ok(Vec::new());
    }
    let count = usize::try_from(self.half()?).map_err(|_| RegisterError::MalformedRecord)?;
    if count == 0 || count > self.remaining() / OBJECT_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let mut objects = Vec::with_capacity(count);
    for _ in 0..count {
      objects.push(self.object()?);
    }
    Ok(objects)
  }

  /// The bytes left to read.
  fn remaining(&self) -> usize {
    self.rest.len()
  }

  /// Refuses bytes left over after the message.
  fn finish(self) -> Result<(), RegisterError> {
    if self.rest.is_empty() {
      Ok(())
    } else {
      Err(RegisterError::MalformedRecord)
    }
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
  /// The owner an install last moved this acceptor's authority from (A-251): a takeover moves a retired owner's copies
  /// to its successor, and only this keeps whose object the copy was once the authority names the successor.
  moved_from: Option<HostId>,
  fence: Fence,
  accepted: std::collections::BTreeMap<(ObjectId, u64), (HostEpoch, Vec<u8>)>,
  /// What changed of `accepted` since [`Acceptor::clear_changes`], kept only by an acceptor whose holder publishes each
  /// position as its own piece ([`Acceptor::tracking_changes`], A-367).
  changes: Option<PositionChanges>,
}

/// What changed of a tracking acceptor's accepted positions since its changes were last cleared (A-367; §4.8
/// "persistence before reply"): the positions accepted — new, or accepted again at a newer epoch — and those dropped —
/// compacted below a newer one, or the object forgotten — and not accepted again since. A holder's recovery image carries
/// each position as its own piece, so its publication puts the positions written and removes those dropped, and never
/// re-encodes a ledger it already carries. Kept only by an acceptor that asked ([`Acceptor::tracking_changes`]): an
/// owner's acceptor, whose positions no image carries, keeps none. Bounded by what changed since the holder's last
/// committed publication, which clears them ([`Acceptor::clear_changes`]); a holder publishes each accept before it
/// acknowledges it, so that is a handful.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PositionChanges {
  /// The positions accepted since the last clear, each as its object and sequence.
  pub written: std::collections::BTreeSet<(ObjectId, u64)>,
  /// The positions dropped since the last clear and not accepted again since, each as its object and sequence.
  pub dropped: std::collections::BTreeSet<(ObjectId, u64)>,
}

impl PositionChanges {
  /// Notes `position` accepted: written, and no longer dropped.
  fn note_written(&mut self, position: (ObjectId, u64)) {
    self.dropped.remove(&position);
    self.written.insert(position);
  }

  /// Notes `position` dropped: dropped, and no longer written.
  fn note_dropped(&mut self, position: (ObjectId, u64)) {
    self.written.remove(&position);
    self.dropped.insert(position);
  }
}

/// A holder's durable accepted positions, for recovery: `(object, sequence, epoch, value)` per entry.
pub type AcceptedPositions = Vec<(ObjectId, u64, HostEpoch, Vec<u8>)>;

/// One acceptor's durable state as a recovery image keeps it (§4.8 "persistence before reply"; A-251): the owner and
/// configuration generation it serves under, the epoch its fence has seen, every position it accepted, and the retired
/// owner an install last moved its authority from. [`Acceptor::to_persisted`] makes it and [`Acceptor::from_persisted`]
/// rebuilds the same acceptor from it, so a restarted holder lists, refuses and accepts as it did before.
///
/// Format: the `Wire` encoding of its fields in this order. The moved-from owner is last, so the layout without it (the
/// held image's register before A-251) is refused as truncated, never read as "moved from none" (CLAUDE.md §2.7); the
/// image's version is what refuses an older image whole.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct PersistedAcceptor {
  /// The owner it serves under ([`Authority::owner`]).
  pub owner: HostId,
  /// The configuration generation it serves under ([`Authority::generation`]).
  pub generation: u64,
  /// The highest epoch its fence has seen.
  pub promised: u64,
  /// Every position it accepted, in key order.
  pub accepted: Vec<PersistedPosition>,
  /// The retired owner an install last moved its authority from ([`Acceptor::moved_from`]).
  pub moved_from: Option<HostId>,
}

/// One accepted position of a [`PersistedAcceptor`]: the object, its ledger position, the epoch it was accepted at and
/// the record's bytes.
#[derive(slates_wire::Wire, Clone, Debug, PartialEq, Eq)]
pub struct PersistedPosition {
  /// The object the record belongs to.
  pub object: [u8; 16],
  /// Its ledger position.
  pub sequence: u64,
  /// The epoch it was accepted at.
  pub epoch: u64,
  /// The record's bytes.
  pub value: Vec<u8>,
}

impl Acceptor {
  /// A fresh acceptor for holder `id` serving under `authority`, having accepted nothing.
  pub fn new(id: HostId, authority: Authority) -> Acceptor {
    Acceptor {
      id,
      authority,
      moved_from: None,
      fence: Fence::new(),
      accepted: std::collections::BTreeMap::new(),
      changes: None,
    }
  }

  /// This acceptor, keeping from now on what changes of its accepted positions (A-367, [`PositionChanges`]): what a
  /// holder whose recovery image carries each position as its own piece publishes from. The positions it already holds
  /// count as written, since no publication is known to carry them; a holder rebuilding it from the image that does
  /// carry them clears them at once ([`Acceptor::clear_changes`]). So a position it holds is either written or carried
  /// by what was last published, and a publication judged by its changes misses none.
  #[must_use]
  pub fn tracking_changes(mut self) -> Acceptor {
    if self.changes.is_none() {
      self.changes = Some(PositionChanges {
        written: self.accepted.keys().copied().collect(),
        dropped: std::collections::BTreeSet::new(),
      });
    }
    self
  }

  /// What changed of its accepted positions since [`Acceptor::clear_changes`]; `None` for an acceptor that keeps none.
  pub fn changes(&self) -> Option<&PositionChanges> {
    self.changes.as_ref()
  }

  /// Forgets what changed of its accepted positions: the holder's publication carrying it committed (A-367).
  pub fn clear_changes(&mut self) {
    if let Some(changes) = self.changes.as_mut() {
      changes.written.clear();
      changes.dropped.clear();
    }
  }

  /// How many positions this acceptor holds, every object's: one read.
  pub fn position_count(&self) -> usize {
    self.accepted.len()
  }

  /// The epoch and value accepted for `object` at `sequence`, if this acceptor holds that position: one lookup.
  pub fn position(&self, object: ObjectId, sequence: u64) -> Option<(HostEpoch, &[u8])> {
    self
      .accepted
      .get(&(object, sequence))
      .map(|(epoch, value)| (*epoch, value.as_slice()))
  }

  /// Every accepted position, in key order, as its object, sequence, epoch and value: what a whole publication carries
  /// one piece each (A-367), without the copy [`Acceptor::persisted`] makes.
  pub fn positions(&self) -> impl Iterator<Item = (ObjectId, u64, HostEpoch, &[u8])> + '_ {
    self
      .accepted
      .iter()
      .map(|(&(object, sequence), (epoch, value))| (object, sequence, *epoch, value.as_slice()))
  }

  /// This acceptor's durable state but its positions (A-367): the owner and generation it serves under, the epoch its
  /// fence has seen and the retired owner an install last moved it from, which a recovery image keeps as the register's
  /// head piece beside a piece per position. [`Acceptor::from_persisted`] rebuilds the acceptor from it with the
  /// positions put back into `accepted`.
  pub fn persisted_head(&self) -> PersistedAcceptor {
    PersistedAcceptor {
      owner: self.authority.owner,
      generation: self.authority.generation,
      promised: self.fence.seen.0,
      accepted: Vec::new(),
      moved_from: self.moved_from,
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
      moved_from: None,
      fence,
      accepted,
      changes: None,
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

  /// The authorized owner this acceptor currently serves under. A holder reads it to raise the fence for a
  /// **departed** owner when it installs a configuration that bumped that host's epoch (see
  /// [`raise_fence`](Acceptor::raise_fence)).
  pub fn owner(&self) -> HostId {
    self.authority.owner
  }

  /// The retired owner an install last moved this acceptor's authority from ([`Acceptor::move_authority`]), `None` when
  /// none has (A-251). Persisted with the acceptor, so a restart keeps it ([`Acceptor::to_persisted`]).
  pub fn moved_from(&self) -> Option<HostId> {
    self.moved_from
  }

  /// This acceptor's whole durable state, its moved-from owner too, as a recovery image keeps it (A-251).
  pub fn to_persisted(&self) -> PersistedAcceptor {
    let (promised, accepted) = self.persisted();
    PersistedAcceptor {
      owner: self.authority.owner,
      generation: self.authority.generation,
      promised: promised.0,
      accepted: accepted
        .into_iter()
        .map(|(object, sequence, epoch, value)| PersistedPosition {
          object: object.0,
          sequence,
          epoch: epoch.0,
          value,
        })
        .collect(),
      moved_from: self.moved_from,
    }
  }

  /// The acceptor holder `id` kept as `persisted` ([`Acceptor::to_persisted`]), rebuilt after a restart.
  pub fn from_persisted(id: HostId, persisted: PersistedAcceptor) -> Acceptor {
    let accepted = persisted
      .accepted
      .into_iter()
      .map(|position| {
        (
          ObjectId(position.object),
          position.sequence,
          HostEpoch(position.epoch),
          position.value,
        )
      })
      .collect();
    let mut acceptor = Acceptor::recovered(
      id,
      Authority {
        generation: persisted.generation,
        owner: persisted.owner,
      },
      HostEpoch(persisted.promised),
      accepted,
    );
    acceptor.moved_from = persisted.moved_from;
    acceptor
  }

  /// The configuration generation this acceptor serves under: what a recovery image keeps with its records, so a
  /// restarted holder refuses and accepts as it did before.
  pub fn generation(&self) -> u64 {
    self.authority.generation
  }

  /// Raises this holder's fence for the object to at least `epoch` (§4.8 "Promotion and takeover": "every
  /// holder raises its fence for that host to the new epoch"). When the configuration group bumps a failed
  /// host's fencing epoch, a holder installing that configuration raises the fence for the objects that host
  /// owned **at once** — before the per-object phase-one prepare — so a resumed stale owner writing under the
  /// old epoch is refused `StaleEpoch` immediately across all of them. Monotonic: a lower epoch is ignored,
  /// so this only ever strengthens the fence (it can never admit a record the fence already refused).
  pub fn raise_fence(&mut self, epoch: HostEpoch) {
    let _ = self.fence.accept(epoch);
  }

  /// Accepts `record` under this holder's authority and fence, storing it before acknowledging, or
  /// refusing with a typed reason. The order is authority (a stale or foreign configuration generation, or
  /// an unauthorized owner, refuses before touching the fence), then the fence (a stale epoch refuses), then
  /// the position (a *different* value at a position already accepted is a conflict — a committed position is
  /// never rewritten; the *same* value re-acknowledges idempotently). On acceptance the promise is raised and
  /// the accepted `(epoch, value)` recorded — recording the new epoch even when the value is unchanged
  /// (BUG-12) — before the [`Ack`] is returned.
  ///
  /// The configuration-generation gate is split by direction, so the sender learns *which way* it is out of
  /// step and reacts (§4.8 the piggyback rule, "a receiver with a newer configuration refuses with
  /// `ConfigurationStale{version}` and the current version"). A record from an **older** configuration than
  /// this holder's is the stale-sender case: refused [`ConfigurationStale`](RegisterError::ConfigurationStale)
  /// naming this holder's current (newer) version, so the sender refreshes its configuration and retries
  /// within the operation's budget. A record from a **newer** configuration is the stale-holder case: this
  /// holder cannot validate authority under a generation it has not installed, so it still refuses
  /// ([`ForeignGeneration`](RegisterError::ForeignGeneration), naming its current generation) — and refreshes
  /// reactively, having now seen a request that carries a newer version.
  pub fn accept(&mut self, record: &Record) -> Result<Ack, RegisterError> {
    self.check(record)?;
    self.fence.accept(record.epoch)?;
    // Store the accepted value and the raised promise before acknowledging.
    self.accepted.insert(
      (record.object, record.sequence),
      (record.epoch, record.value.clone()),
    );
    if let Some(changes) = self.changes.as_mut() {
      changes.note_written((record.object, record.sequence));
    }
    Ok(Ack {
      holder: self.id,
      object: record.object,
      sequence: record.sequence,
      generation: record.generation,
      identity: record.identity(),
    })
  }

  /// Drops every accepted position of `object` below `sequence` (AUD-29-43). For a **single-value** register
  /// — a volume's head, its catalog, a tombstone — phase one reports only the highest accepted position
  /// ([`prepare`](Acceptor::prepare)), so once `sequence` is accepted the older positions can never be adopted
  /// and keeping them is unbounded growth, one position per seal. A **ledger** (a green's merge chain) must
  /// not be compacted: a successor replays every version. The owner of the value class decides which it is.
  pub fn compact_below(&mut self, object: ObjectId, sequence: u64) {
    let changes = &mut self.changes;
    self.accepted.retain(|&(held, position), _| {
      let kept = held != object || position >= sequence;
      if !kept && let Some(changes) = changes.as_mut() {
        changes.note_dropped((held, position));
      }
      kept
    });
  }

  /// The highest accepted position of `object` — by sequence, then epoch, as phase one ranks them — as its
  /// sequence and value, or `None` when nothing of it is accepted.
  pub fn highest(&self, object: ObjectId) -> Option<(u64, &[u8])> {
    self
      .accepted
      .range((object, 0)..=(object, u64::MAX))
      .next_back()
      .map(|(&(_, sequence), (_, value))| (sequence, value.as_slice()))
  }

  /// The highest record this acceptor holds of `object` as phase one reports it ([`Accepted`]: its position, the epoch it
  /// was accepted at and its value), or `None` when it holds none. What the member a move left answers for its own
  /// object from its owner's acceptor (A-291).
  pub fn highest_accepted(&self, object: ObjectId) -> Option<Accepted> {
    self
      .accepted
      .range((object, 0)..=(object, u64::MAX))
      .next_back()
      .map(|(&(_, sequence), (epoch, value))| Accepted {
        sequence,
        epoch: *epoch,
        value: value.clone(),
      })
  }

  /// The value accepted for `object` at `sequence`, if this acceptor holds that position: one lookup, where
  /// [`Acceptor::values`] walks every position the object holds.
  pub fn value_at(&self, object: ObjectId, sequence: u64) -> Option<&[u8]> {
    self
      .accepted
      .get(&(object, sequence))
      .map(|(_, value)| value.as_slice())
  }

  /// Every accepted value of `object`, in sequence order (a ledger's whole history).
  pub fn values(&self, object: ObjectId) -> impl Iterator<Item = &[u8]> + '_ {
    self
      .accepted
      .range((object, 0)..=(object, u64::MAX))
      .map(|(_, (_, value))| value.as_slice())
  }

  /// Drops every accepted position of `object` (AUD-29-43): the object's registers are retired — a destroyed
  /// volume whose tombstone every candidate holds — so nothing of it can be adopted again.
  pub fn forget_object(&mut self, object: ObjectId) {
    let changes = &mut self.changes;
    self.accepted.retain(|&(held, position), _| {
      let kept = held != object;
      if !kept && let Some(changes) = changes.as_mut() {
        changes.note_dropped((held, position));
      }
      kept
    });
  }

  /// Checks authority, epoch and position without changing the promise or accepted history (§4.8).
  /// A holder uses this before recomputing a merge record: a refused record cannot mutate its replica.
  /// The check and acceptance must run in the same owning-shard turn, with no intervening await.
  pub fn check(&self, record: &Record) -> Result<(), RegisterError> {
    if record.generation < self.authority.generation {
      return Err(RegisterError::ConfigurationStale {
        version: self.authority.generation,
      });
    }
    if record.generation > self.authority.generation {
      return Err(RegisterError::ForeignGeneration {
        current: self.authority.generation,
      });
    }
    if record.owner != self.authority.owner {
      return Err(RegisterError::Unauthorized);
    }
    if record.epoch < self.fence.seen {
      return Err(RegisterError::StaleEpoch {
        current: self.fence.seen.0,
      });
    }
    let position = (record.object, record.sequence);
    if let Some((_, existing)) = self.accepted.get(&position)
      && existing != &record.value
    {
      return Err(RegisterError::ConflictingPosition);
    }
    Ok(())
  }

  /// The authority this acceptor serves now: the generation and the owner allowed to write.
  pub fn authority(&self) -> Authority {
    self.authority
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

  /// Installs `authority` as [`install_authority`](Acceptor::install_authority) does, and when it moves the authority
  /// away from an owner the caller knows is retired (`from_retired`: a takeover moving a retired owner's copy to its
  /// successor), records that owner as [`moved_from`](Acceptor::moved_from) (A-251). One owner, overwritten by the next
  /// such move, never a growing list; an install that changes no owner, or moves from a live one, leaves it as it was.
  pub fn move_authority(
    &mut self,
    authority: Authority,
    from_retired: bool,
  ) -> Result<(), RegisterError> {
    let previous = self.authority.owner;
    self.install_authority(authority)?;
    if from_retired && previous != authority.owner {
      self.moved_from = Some(previous);
    }
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
    joint: Vec::new(),
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
  /// The failure domain of each fleet host, so copysets are formed across **distinct** domains (D-14: "a
  /// fixed set of hosts across failure domains") — a rack or machine failure then takes at most one copy of
  /// any object. A host absent from the map is its **own** domain (`host.0`, unique-per-host), the safe
  /// default a laptop and an undeclared deployment keep; a deployment populates it from the manifest so
  /// co-located hosts share a domain. Consumed only by [`candidates_for`], never on a read path.
  pub domains: std::collections::BTreeMap<HostId, DomainId>,
  /// The quorum the fault-domain tree fixes (`f = 0` on a laptop).
  pub quorum: Quorum,
  /// Whether a mirror region exists.
  pub has_mirror: bool,
  /// While this owner's neighbourhood change is in flight, the neighbourhood its records are all placed on
  /// ([`Settled`]): its records commit at `f + 1` of both cohorts ([`place`](Configuration::place)) until the
  /// council settles the current one, and a takeover recovers through this one. `None` once its current
  /// neighbourhood is settled — always on a laptop.
  pub settled: Option<Settled>,
  /// The configuration version this owner's current neighbourhood was fixed at ([`Neighbourhood::generation`]):
  /// with the settled neighbourhood's, the owner's authority standing ([`Configuration::standing`]).
  pub generation: u64,
}

/// An owner's **authority standing** as its installed configuration fixes it (§4.8 "Leases and reads": "a
/// majority observation must belong to the relevant authority generation"): the versions its settled and its
/// current neighbourhoods were fixed at. A takeover of the owner recovers through its settled neighbourhood
/// ([`Retirement::settled`]), and that is either the one this owner holds settled or — once the council takes
/// the owner's report — its current one. So a holder's view belongs to the owner's authority generation when
/// its configuration fixes the owner's settled neighbourhood at either version; every other configuration
/// change — another host's admission, settlement, retirement, a confirmed takeover — leaves it as it was.
/// Equal on a settled owner; zero on a laptop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Standing {
  /// The version the owner's settled neighbourhood was fixed at.
  pub settled: u64,
  /// The version the owner's current neighbourhood was fixed at: its settled one once the council takes the
  /// owner's report, never older than `settled`.
  pub current: u64,
}

impl Standing {
  /// Whether a holder whose configuration fixes this owner's settled neighbourhood at `generation` sees the
  /// owner under this standing: its settled neighbourhood, or its current one the council has settled since.
  pub fn recognizes(self, generation: u64) -> bool {
    generation == self.settled || generation == self.current
  }

  /// Whether a holder's settled generation for this owner is one this standing cannot account for: newer than
  /// the owner's current neighbourhood, so the owner's authority changed in a configuration it has not
  /// installed (its id retired and admitted again).
  pub fn superseded_by(self, generation: u64) -> bool {
    generation > self.current
  }
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
      // Unique-per-host by default: no host is listed, so each is its own failure domain.
      domains: std::collections::BTreeMap::new(),
      quorum: Quorum { f: 0 },
      has_mirror: false,
      settled: None,
      generation: 0,
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

  /// The placement a **record** of `object` takes when the owner first holds it (`Placement::local`): its
  /// cohort in the current neighbourhood — and, while a change is in flight and moved this object's cohort,
  /// the settled cohort too, joined (§4.8 "joint writes"): the record goes to both and commits at `f + 1` of
  /// each, so every record the owner commits stays at `f + 1` of the cohort a takeover recovers through (the
  /// settled one, or the current one once the council settles it: [`lease_cohorts`](Configuration::lease_cohorts)).
  pub fn place(&self, object: ObjectId) -> Placement {
    let mut placement = self.place_content(object);
    if let Some(settled) = &self.settled {
      let old = settled.cohort(self.owner, object, self.quorum);
      if old != placement.candidates {
        let current = placement.candidates.clone();
        for host in &old {
          if !placement.candidates.contains(host) {
            placement.candidates.push(*host);
          }
        }
        placement.joint = vec![old, current];
      }
    }
    placement
  }

  /// Whether a record of `object` acknowledged by `acked` is held by `f + 1` of its cohort in the current
  /// neighbourhood — the readiness §4.8 "Neighbourhood changes" names for the old set to retire ("the newest
  /// committed record is held by f+1 of the new candidates"). The old cohort is deliberately not asked: every
  /// record committed while the change is in flight already reached `f + 1` of it, and once the council settles
  /// the current neighbourhood a takeover recovers through that one, so its holding the newest record is the
  /// whole of the state transfer. Judging the joint shape instead kept an owner whose old cohort lost a host
  /// unsettled for good — that cohort can never again gather `f + 1` — and so every write it made unplaced.
  pub fn placed_on_current(&self, object: ObjectId, acked: &[HostId]) -> bool {
    self.place_content(object).placed_with(acked, self.quorum)
  }

  /// The placement **content** of `object` takes (§4.10): its cohort in the current neighbourhood alone. Content
  /// is fetched by identity from the holders the head names, so it needs no joint placement across a change.
  pub fn place_content(&self, object: ObjectId) -> Placement {
    Placement::local(
      self.owner,
      self.quorum,
      &self.neighbourhood,
      &self.domains,
      object,
    )
  }

  /// The cohorts this owner's lease over `object` must hold confirmations in (§4.8 "Leases and reads"): the
  /// one a takeover recovers `object` through — the settled cohort — and, while a change is in flight that
  /// moved `object`'s cohort, the current one too, since the council may settle it at any moment and a
  /// retirement after that recovers through it. The joint lease mirrors the joint writes
  /// ([`place`](Configuration::place)): counting only the settled cohort let an owner cut off with holders
  /// that had not yet learned its settlement keep its lease while a successor recovered through the new cohort.
  /// One cohort outside a change, or when the change left `object`'s cohort as it was.
  pub fn lease_cohorts(&self, object: ObjectId) -> Vec<Vec<HostId>> {
    let placement = self.place(object);
    if placement.joint.is_empty() {
      vec![placement.candidates]
    } else {
      placement.joint
    }
  }

  /// Whether `host` is in this owner's neighbourhood for placement: its current one, or — while a change is in
  /// flight — its settled one, which its joint writes still reach ([`place`](Configuration::place)) and its
  /// joint lease still counts ([`lease_cohorts`](Configuration::lease_cohorts)).
  pub fn neighbours(&self, host: HostId) -> bool {
    self.neighbourhood.contains(&host)
      || self
        .settled
        .as_ref()
        .is_some_and(|settled| settled.hosts.contains(&host))
  }

  /// The hosts of this owner's neighbourhood, current and — while a change is in flight — settled, the owner
  /// excluded, each once, appended to `into`: every host a lease cohort can draw from
  /// ([`lease_cohorts`](Configuration::lease_cohorts)), so every host whose answers confirm this owner's lease.
  pub fn lease_holders_into(&self, into: &mut Vec<HostId>) {
    let settled = self
      .settled
      .as_ref()
      .map(|settled| settled.hosts.as_slice());
    for host in self.neighbourhood.iter().chain(settled.unwrap_or_default()) {
      if *host != self.owner && !into.contains(host) {
        into.push(*host);
      }
    }
  }

  /// This owner's authority standing ([`Standing`]): the versions its settled and its current neighbourhoods
  /// were fixed at.
  pub fn standing(&self) -> Standing {
    Standing {
      settled: self
        .settled
        .as_ref()
        .map_or(self.generation, |settled| settled.generation),
      current: self.generation,
    }
  }

  /// The number of distinct **copysets** this owner's objects spread over under the current neighbourhood
  /// and failure domains (§4.8, D-14): the actual [`owner_copysets`] partition — one at the candidate
  /// floor, linear in the scatter width above it, and larger when the domains are too poor to fill the
  /// copysets. The count a durability check reads at every configuration change (research §3.1).
  pub fn copyset_count(&self) -> usize {
    let with_domains: Vec<(HostId, DomainId)> = self
      .neighbourhood
      .iter()
      .map(|host| (*host, self.domains.get(host).copied().unwrap_or(host.0)))
      .collect();
    owner_copysets(self.owner, &with_domains, self.quorum).len()
  }

  /// The probability that a coincident failure of `failed` hosts among the neighbourhood loses some object:
  /// `#copysets · C(failed, R) / C(H, R)` with `H` the neighbourhood size and `R = f + 1` content copies
  /// (research §3.1; [A: Cidon et al., ATC 2013]). Monotone in the copyset count at a fixed neighbourhood
  /// size, so the bounded, linear-count, domain-distinct neighbourhood is the low-loss one; at `f = 0` (the
  /// laptop) it is the owner's own loss.
  pub fn coincident_loss(&self, failed: u64) -> f64 {
    let hosts = u64::try_from(self.neighbourhood.len()).unwrap_or(u64::MAX);
    let copysets = u64::try_from(self.copyset_count()).unwrap_or(u64::MAX);
    // A bootstrapping configuration cannot claim protection from hosts not yet admitted.
    let copies = u64::from(self.quorum.f).saturating_add(1).min(hosts);
    coincident_loss_probability(copysets, hosts, failed, copies)
  }

  /// Whether the configuration keeps its coincident-loss probability within the operator's accepted
  /// `epsilon` under a coincident failure of `failed` hosts (§4.8: the copyset count checked against its
  /// durability bound at every configuration change; a breach is a recovery-vs-durability conflict to
  /// resolve — more copies, more bandwidth, or tighter failure domains — never a silent over-scatter).
  pub fn within_loss_bound(&self, epsilon: f64, failed: u64) -> bool {
    self.coincident_loss(failed) <= epsilon
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

/// A host's bounded neighbourhood as the regional council fixes it (§4.8, D-14): the scatter set across
/// failure domains, and the **generation** at which that host set last changed — so a membership change moves
/// only the hosts whose rendezvous rank crossed the cut ("add before remove", stable), and an owner can tell
/// whether its neighbourhood moved since the neighbourhood its records are all placed on ([`Settled`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Neighbourhood {
  /// The owner plus its scatter-width co-holders across failure domains (the owner heads it).
  pub hosts: Vec<HostId>,
  /// The configuration version at which this host set was last changed: a refix that leaves the set as it
  /// was keeps the version it had, so a neighbourhood whose version is unchanged is the same set.
  pub generation: u64,
}

/// A neighbourhood an owner's records are all placed on (§4.8 "Neighbourhood changes"; Vertical Paxos II's
/// complete configuration [A: Lamport, Malkhi, Zhou, "Vertical Paxos and Primary-Backup Replication", PODC
/// 2009]): while an owner's current neighbourhood differs from it, the owner writes jointly to both, and the
/// council moves it forward only on the owner's report that every object it owns is placed at `f + 1` of the
/// new cohort ([`RegionalConfiguration::settle`]). So every record an owner ever committed is held by `f + 1` of
/// its settled cohort, and a takeover recovers through that cohort. It keeps each host's failure domain, so the
/// cohorts drawn from it are recomputed exactly after some of its hosts retired and left the domain map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
  /// The neighbourhood's hosts, the owner among them.
  pub hosts: Vec<HostId>,
  /// The configuration version at which this host set was fixed ([`Neighbourhood::generation`]).
  pub generation: u64,
  /// The declared failure domain of each of those hosts that has one; a host absent is its own domain, as in
  /// [`candidates_for`].
  pub domains: std::collections::BTreeMap<HostId, DomainId>,
}

impl Settled {
  /// The settled form of `neighbourhood`: its hosts and version, with the declared domain of each host.
  fn of(
    neighbourhood: &Neighbourhood,
    domains: &std::collections::BTreeMap<HostId, DomainId>,
  ) -> Settled {
    Settled {
      hosts: neighbourhood.hosts.clone(),
      generation: neighbourhood.generation,
      domains: neighbourhood
        .hosts
        .iter()
        .filter_map(|host| domains.get(host).map(|domain| (*host, *domain)))
        .collect(),
    }
  }

  /// The cohort `owner` placed `object` on in this neighbourhood: the same [`candidates_for`] the owner
  /// computed when it wrote, over the same hosts and domains.
  pub fn cohort(&self, owner: HostId, object: ObjectId, quorum: Quorum) -> Vec<HostId> {
    candidates_for(owner, &self.hosts, &self.domains, object, quorum)
  }
}

/// A retired host's retirement (§4.8 "Promotion and takeover"): what the region needs to recover its objects
/// the same way on every survivor, kept until every survivor that could own a share of them has finished it, when it
/// becomes a [`CompletedTakeover`] (A-309).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retirement {
  /// The configuration version that retired the host.
  pub version: u64,
  /// The neighbourhood its records were all placed on when it retired: its objects' recovery cohort.
  pub settled: Settled,
  /// Its current neighbourhood when it retired, while a change was in flight (A-309): the hosts its joint writes and
  /// re-shipped heads reached besides the settled ones. `None` when it retired settled on its current neighbourhood.
  /// Its members are survivors too, and asked in the takeover's round ([`RegionalConfiguration::recovery_hosts`]): a
  /// member that joined the neighbourhood after it was settled can hold a copy no settled host still holds (the
  /// replacement holding the live log, in the store-destroyed takeover).
  pub current: Option<Settled>,
  /// The members of its recovery neighbourhoods when it retired, itself excluded, in id order: the hosts its
  /// objects' successors are ranked among, fixed at the retirement so every node ranks them the same whatever
  /// it has installed since, and the hosts that owe a confirmation of their share while they remain members. Its
  /// recovery neighbourhoods are its settled and current ones and those of the takeovers it carries.
  pub survivors: Vec<HostId>,
  /// The survivors that have confirmed their share of this takeover — every object of the retired host that
  /// ranks them first adopted and placed under their own placement — in id order.
  pub confirmed: Vec<HostId>,
  /// The retired hosts whose takeover this host had not confirmed its share of when it retired, in id order:
  /// an object it was taking over from one of them may not have been re-committed under it yet, so its own
  /// objects are recovered jointly with those hosts' cohorts ([`RegionalConfiguration::recovery_owners`]).
  pub unconfirmed: Vec<HostId>,
}

/// Where a retired owner's object belongs ([`RegionalConfiguration::lineage`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lineage {
  /// `successor`, a member, takes the object over as part of `departed`'s takeover — `departed` the last
  /// retired owner in the object's line, the one whose round the successor adopts it in.
  Successor {
    /// The retired host whose takeover the object is recovered in.
    departed: HostId,
    /// The member that takes it over.
    successor: HostId,
  },
  /// A retirement in the object's line has no survivor to rank: no member was left in any neighbourhood that could
  /// hold its records, so nothing can recover it.
  Lost,
  /// The object's line reaches `takeover`, a takeover the configuration keeps a completion record of
  /// ([`RegionalConfiguration::completed`]): every survivor confirmed its share, or left and carried it into a takeover
  /// of its own that completed, and a survivor confirms only once every object it learned was adopted (A-272). Positive
  /// evidence, never inferred from a retirement no longer kept (A-309). A holder that took part in that takeover holds a
  /// stale copy: its answers taught the successors every copy it held, so each was adopted
  /// ([`RegionalConfiguration::settled_for`]); a holder outside it was never asked, and nothing shows its copy adopted.
  Settled {
    /// The retired host whose completed takeover the line ends at.
    takeover: HostId,
  },
  /// Nothing shows where the object went (A-272, A-309): its line reaches a retired host the configuration keeps no
  /// record of (a retirement the bound dropped before it completed, or a completion record no longer kept), or a kept
  /// retirement ranks a successor that is neither a member, nor a kept retirement, nor a completed takeover. Not
  /// settled: absence is no evidence of adoption, and a holder keeps its copy.
  Unresolved,
}

/// A takeover every survivor of which finished its share (A-309): the record [`Lineage::Settled`] rests on, kept in
/// [`RegionalConfiguration::completed`] when the retirement is dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedTakeover {
  /// The configuration version the takeover completed at: the order the records are let go in, oldest first.
  pub version: u64,
  /// The survivors that confirmed their share before the takeover completed, in id order: each adopted and placed every
  /// object of the retired host that ranked it first, so a holder outside such a successor's placement holds a stale
  /// copy (`stale_after_confirmation` in the server's takeover). A survivor absent here left before confirming and
  /// carried its share into its own takeover, which completed too.
  pub confirmed: Vec<HostId>,
}

/// The **regional configuration** the council agrees on (§4.8, D-14 — the "configuration master", a small
/// elected council per region): the whole region's membership, each owner's bounded neighbourhood, each
/// host's fencing epoch, and the failure-domain map. The council decides these — on membership, takeover,
/// neighbourhood and home changes only, never per write — and every node learns them; a node derives its
/// own single-owner [`Configuration`] for the placement path with [`configuration_for`](
/// RegionalConfiguration::configuration_for). This is the multi-owner form the design specifies
/// (`neighbourhoods` keyed by owner); the single-owner [`Configuration`] is one owner's slice of it.
/// `version` advances on every change, so a request under a stale version is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionalConfiguration {
  /// Advances on every membership, neighbourhood, epoch or takeover change (near-zero rate outside failures).
  pub version: u64,
  /// The members the council agrees are in the region (the alive set it has admitted), in id order.
  pub members: Vec<HostId>,
  /// Each member's bounded neighbourhood — the scatter set the placement path draws candidates from.
  pub neighbourhoods: std::collections::BTreeMap<HostId, Neighbourhood>,
  /// Each host's fencing epoch (bumped on takeover, so a resumed stale owner's records are refused).
  pub epochs: std::collections::BTreeMap<HostId, HostEpoch>,
  /// Each host's failure domain (unique-per-host when absent), so copysets form across distinct domains.
  pub domains: std::collections::BTreeMap<HostId, DomainId>,
  /// The quorum the failure-domain tree fixes (`f = 0` on a laptop).
  pub quorum: Quorum,
  /// Whether a mirror region exists.
  pub has_mirror: bool,
  /// Each member's settled neighbourhood ([`Settled`]): the set its records are all placed on — its current
  /// neighbourhood outside a change in flight, the previous one until it reports the change placed
  /// ([`settle`](RegionalConfiguration::settle)). Bounded by the members.
  pub settled: std::collections::BTreeMap<HostId, Settled>,
  /// Each retired host's [`Retirement`], kept until every survivor that could own a share of its objects has
  /// confirmed that share ([`confirm`](RegionalConfiguration::confirm)), when it becomes a record in
  /// [`completed`](RegionalConfiguration::completed), and never more than
  /// [`retirement_bound`](RegionalConfiguration::retirement_bound) of them. One no survivor owes anything of that
  /// cannot complete (a survivor left and its own takeover never completed, or it had no survivor at all) stays kept,
  /// so its lines still name what is known, until the bound lets it go first.
  pub retired: std::collections::BTreeMap<HostId, Retirement>,
  /// The promoted objects this region adopts from the region it mirrors (§4.10 "region loss promotes the mirror";
  /// `docs/wip/mirroring.md` decision 4): each object's agreed successor, with the head and catalog records it adopts.
  /// An entry is assigned by the council leader after its phase one and dropped once its successor reports it serves
  /// the object ([`adopted`](RegionalConfiguration::adopted)), so it holds only adoptions in flight.
  pub adoptions: std::collections::BTreeMap<ObjectId, Adoption>,
  /// The takeovers that completed (A-309, A-272 (ii)), each retired host's [`CompletedTakeover`]: the only evidence a
  /// line reads [`Lineage::Settled`] from, so a holder lets a copy go only on a record that its objects were adopted,
  /// never on a retirement's absence. A retirement becomes a record when every survivor confirmed its share or left and
  /// carried it into a takeover of its own that completed ([`completes`](RegionalConfiguration::completes)). At most
  /// [`completed_bound`](RegionalConfiguration::completed_bound) are kept, or the ones kept retirements still need when
  /// those are more, the oldest let go first; a line reaching a host whose record was let go reads
  /// [`Lineage::Unresolved`], so a holder that lagged past it keeps a stale copy rather than reclaim one nobody adopted.
  pub completed: std::collections::BTreeMap<HostId, CompletedTakeover>,
  /// The objects an explicit move took off their creator's line (§4.8 "Authority scope": "moving one volume changes
  /// that object's ownership generation, recorded in the configuration's moved-object exceptions, without fencing
  /// unrelated volumes"; §4.10 "operators keep an explicit move"), keyed by the object's placement key, so every register
  /// class of a volume moves as one ([`ObjectId::placement_key`]). Moved objects only: an object absent here is owned
  /// along its creator's line ([`owner_of`](RegionalConfiguration::owner_of)). An entry is made by its owner's move
  /// ([`move_object`](RegionalConfiguration::move_object)), a move back to its creator included (the creator released
  /// its copy when the object left, so it adopts the object back as any member does, A-291), follows its owner's
  /// takeover at the successor's confirmation, and is dropped when that successor is the creator or its owner reports
  /// it destroyed ([`forget_moved`](RegionalConfiguration::forget_moved)), so it holds only live moved objects; a move
  /// past the bound its proposer derives is refused before it is proposed.
  pub moved: std::collections::BTreeMap<ObjectId, Moved>,
}

/// One object's explicit move off its creator's line ([`RegionalConfiguration::moved`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
  /// The member the object moved to: its owner from then on, and the host its line starts from when that member
  /// retires.
  pub owner: HostId,
  /// The configuration version the move committed at: the object's ownership generation (§4.8 "Authority scope").
  pub version: u64,
  /// The member an explicit move took the object from, its owner until the move (A-291). The new owner adopts the
  /// object by phase one over its cohort without counting it: it released its copy at the switch, and its own
  /// acknowledgement may be one of the `f + 1` that placed the newest records, so its answer could say nothing of a
  /// record the others hold. `None` when the exception followed a takeover, whose successor adopted the object in the
  /// takeover's own round. Cleared once the new owner reports the object adopted
  /// ([`adopt_move`](RegionalConfiguration::adopt_move)), which is when the member it left lets its copy go.
  pub from: Option<HostId>,
  /// The cohort the move's records were placed in: the candidates the member it left held the object's head and
  /// catalog at `f + 1` of when it reported the move, itself among them (A-291). The new owner's phase one asks these
  /// hosts by name and counts them, whatever its own placement of the object is, so the two need not be one set. In id
  /// order; empty once `from` is.
  pub cohort: Vec<HostId>,
}

/// One promoted object's adoption in the mirror region (`docs/wip/mirroring.md` decision 4): the member that adopts it,
/// and the newest head and catalog records phase one found for it, each an encoded [`Record`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adoption {
  /// The member that adopts and serves the object.
  pub successor: HostId,
  /// The adopted head record.
  pub head: Vec<u8>,
  /// The adopted catalog record.
  pub catalog: Vec<u8>,
}

impl RegionalConfiguration {
  /// The regional configuration a fleet forms with (§4.8, boot): every member at the first epoch, each with
  /// its bounded neighbourhood computed at generation zero. `scatter` bounds every neighbourhood (the
  /// candidate floor `2f+1` unless recovery has sized it wider); `domains` gives each host's failure domain.
  pub fn formed(
    members: Vec<HostId>,
    quorum: Quorum,
    domains: std::collections::BTreeMap<HostId, DomainId>,
    scatter: u64,
    has_mirror: bool,
  ) -> RegionalConfiguration {
    let mut members = members;
    members.sort_unstable_by_key(|host| host.0);
    let epochs = members.iter().map(|&host| (host, FIRST_EPOCH)).collect();
    let mut config = RegionalConfiguration {
      version: 0,
      members,
      neighbourhoods: std::collections::BTreeMap::new(),
      epochs,
      domains,
      quorum,
      has_mirror,
      settled: std::collections::BTreeMap::new(),
      retired: std::collections::BTreeMap::new(),
      adoptions: std::collections::BTreeMap::new(),
      completed: std::collections::BTreeMap::new(),
      moved: std::collections::BTreeMap::new(),
    };
    config.fix_neighbourhoods(scatter);
    // A formed region owns nothing yet: every member's first neighbourhood is already settled.
    config.settled = config
      .neighbourhoods
      .iter()
      .map(|(owner, neighbourhood)| (*owner, Settled::of(neighbourhood, &config.domains)))
      .collect();
    config
  }

  /// Fixes every member's bounded neighbourhood from the current membership (`select_neighbourhood`): the
  /// owner plus its scatter-width co-holders by rendezvous. A neighbourhood whose host set the change left as it
  /// was keeps the version it was fixed at; one that moved takes the current version.
  fn fix_neighbourhoods(&mut self, scatter: u64) {
    let version = self.version;
    let members = self.members.clone();
    let previous = std::mem::take(&mut self.neighbourhoods);
    self.neighbourhoods = members
      .iter()
      .map(|&owner| {
        let hosts = select_neighbourhood(owner, &members, scatter);
        let generation = previous
          .get(&owner)
          .filter(|unchanged| unchanged.hosts == hosts)
          .map_or(version, |unchanged| unchanged.generation);
        (owner, Neighbourhood { hosts, generation })
      })
      .collect();
  }

  /// The single-owner [`Configuration`] `owner` reads for placement — its bounded neighbourhood, its host
  /// epoch, the failure domains and the quorum — derived from the regional configuration. `None` if `owner`
  /// is not a member.
  pub fn configuration_for(&self, owner: HostId) -> Option<Configuration> {
    let neighbourhood = self.neighbourhoods.get(&owner)?;
    Some(Configuration {
      version: self.version,
      owner,
      host_epoch: self.epochs.get(&owner).copied().unwrap_or(FIRST_EPOCH),
      neighbourhood: neighbourhood.hosts.clone(),
      domains: self.domains.clone(),
      quorum: self.quorum,
      has_mirror: self.has_mirror,
      settled: self
        .settled
        .get(&owner)
        .filter(|settled| settled.generation != neighbourhood.generation)
        .cloned(),
      generation: neighbourhood.generation,
    })
  }

  /// Whether `host` holds `owner`'s content: it is in `owner`'s neighbourhood, current or — while `owner`'s change is
  /// in flight — settled. The holder's side of [`Configuration::neighbours`], asked without building `owner`'s
  /// configuration, for a contact rule asked per probe event (2026-10-10: a holder that kept no contact with an owner
  /// left the pair's probe plane unkeyed when it was the pair's canonical dialer).
  pub fn holds_for(&self, owner: HostId, host: HostId) -> bool {
    let Some(neighbourhood) = self.neighbourhoods.get(&owner) else {
      return false;
    };
    neighbourhood.hosts.contains(&host)
      || self
        .settled
        .get(&owner)
        .filter(|settled| settled.generation != neighbourhood.generation)
        .is_some_and(|settled| settled.hosts.contains(&host))
  }

  /// The version this configuration fixed `host`'s settled neighbourhood at — its authority generation as a
  /// holder sees it ([`Standing`]) — or `None` when `host` is no member (not yet admitted here, or retired).
  pub fn standing_of(&self, host: HostId) -> Option<u64> {
    if !self.members.contains(&host) {
      return None;
    }
    self.settled.get(&host).map(|settled| settled.generation)
  }

  /// Admits `member` to the region (a join the council agrees on) with the failure `domain` its node
  /// declares, if any (`None` leaves it unique-per-host): a no-op if already a member, or while a kept
  /// retirement still names it ([`admits`](RegionalConfiguration::admits)). Refixes the neighbourhoods and
  /// advances the version. Returns whether the membership changed.
  pub fn admit(&mut self, member: HostId, domain: Option<DomainId>, scatter: u64) -> bool {
    if !self.admits(member) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.members.push(member);
    self.members.sort_unstable_by_key(|host| host.0);
    self.epochs.entry(member).or_insert(FIRST_EPOCH);
    if let Some(domain) = domain {
      self.domains.insert(member, domain);
    }
    // A record of an earlier takeover of this id says nothing of the member it names now (A-309): kept, a later
    // takeover of it that never completed would read as completed, settling lines nobody adopted.
    self.completed.remove(&member);
    self.version = version;
    self.fix_neighbourhoods(scatter);
    // An admitted member owns nothing yet, so its first neighbourhood is settled at once. The members whose
    // neighbourhoods it entered keep their settled ones until they report the change placed.
    if let Some(neighbourhood) = self.neighbourhoods.get(&member) {
      let settled = Settled::of(neighbourhood, &self.domains);
      self.settled.insert(member, settled);
    }
    true
  }

  /// Retires `member` from the region (a departure the council agrees on). Refixes the neighbourhoods and
  /// advances the version. Its epoch is kept, so a record from the retired host under an old epoch is still
  /// fenced if it returns; its failure domain is dropped — only members are placed across domains, and an
  /// admission carries a returning node's domain afresh — so the domain map stays bounded to the members
  /// while every restart admits a new member id (task #22; banned item 8). Its settled neighbourhood moves into
  /// a [`Retirement`], with the takeovers it had not confirmed its share of, so its objects are recovered the
  /// same way on every survivor. Returns whether the membership changed.
  pub fn retire(&mut self, member: HostId, scatter: u64) -> bool {
    if !self.members.contains(&member) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    let settled = self.settled.remove(&member).or_else(|| {
      self
        .neighbourhoods
        .get(&member)
        .map(|neighbourhood| Settled::of(neighbourhood, &self.domains))
    });
    // The neighbourhood a change in flight moved it to, which its joint writes reached (A-309).
    let current = self
      .neighbourhoods
      .get(&member)
      .filter(|neighbourhood| {
        settled
          .as_ref()
          .is_some_and(|settled| settled.generation != neighbourhood.generation)
      })
      .map(|neighbourhood| Settled::of(neighbourhood, &self.domains));
    let unconfirmed: Vec<HostId> = self
      .retired
      .keys()
      .copied()
      .filter(|departed| self.owes_confirmation(*departed, member))
      .collect();
    // The hosts the retiring member's objects may be held by — its settled and current neighbourhoods and those of
    // the takeovers it carries — that are members now: its successors are ranked among these for good.
    let survivors = settled.as_ref().map(|settled| {
      let mut survivors: Vec<HostId> = settled
        .hosts
        .iter()
        .chain(current.iter().flat_map(|current| current.hosts.iter()))
        .copied()
        .chain(
          unconfirmed
            .iter()
            .flat_map(|carried| self.recovery_hosts(*carried)),
        )
        .filter(|host| *host != member && self.members.contains(host))
        .collect();
      survivors.sort_unstable_by_key(|host| host.0);
      survivors.dedup();
      survivors
    });
    self.members.retain(|host| *host != member);
    self.neighbourhoods.remove(&member);
    self.domains.remove(&member);
    self.version = version;
    self.fix_neighbourhoods(scatter);
    // Retired with no neighbourhood to recover through, nothing is kept: no record says its objects were adopted, so
    // their lines read unresolved (A-272, A-309).
    if let (Some(settled), Some(survivors)) = (settled, survivors) {
      self.retired.insert(
        member,
        Retirement {
          version: self.version,
          settled,
          current,
          survivors,
          confirmed: Vec::new(),
          unconfirmed,
        },
      );
    }
    self.complete_retirements();
    self.bound_retirements();
    self.bound_completed();
    true
  }

  /// Records that `owner` reports every object it owns placed at `f + 1` of the cohorts of its neighbourhood
  /// fixed at `generation` (§4.8 "the group retires the old set only after the owner has acknowledged the new
  /// configuration and the newest committed record is held by f+1 of the new candidates"): that neighbourhood
  /// becomes its settled one and the version advances, so every node learns the old set is retired. A report
  /// for a neighbourhood that has moved on since, from a non-member, or already recorded changes nothing.
  /// Returns whether the configuration changed.
  pub fn settle(&mut self, owner: HostId, generation: u64) -> bool {
    if !self.settles(owner, generation) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    let Some(neighbourhood) = self.neighbourhoods.get(&owner) else {
      return false;
    };
    let settled = Settled::of(neighbourhood, &self.domains);
    self.settled.insert(owner, settled);
    self.version = version;
    true
  }

  /// Whether `owner`'s report of its neighbourhood fixed at `generation` would move its settled neighbourhood
  /// ([`settle`](RegionalConfiguration::settle)): `owner` is a member, its current neighbourhood is still the one
  /// fixed at `generation`, and that is not already its settled one.
  pub fn settles(&self, owner: HostId, generation: u64) -> bool {
    self
      .neighbourhoods
      .get(&owner)
      .is_some_and(|neighbourhood| neighbourhood.generation == generation)
      && self
        .settled
        .get(&owner)
        .is_none_or(|settled| settled.generation != generation)
  }

  /// Records that `successor` confirms its share of `departed`'s takeover: every object of `departed` that
  /// ranks it first ([`successor`](RegionalConfiguration::successor)) is adopted and placed under its own
  /// placement. The version advances, so every node learns that those objects no longer need `departed`'s
  /// cohort, and the retirement becomes a completion record once every survivor has finished its share
  /// ([`completes`](RegionalConfiguration::completes)). A confirmation from a non-member, from a host that owes none,
  /// or already recorded changes nothing. Returns whether the configuration changed.
  pub fn confirm(&mut self, departed: HostId, successor: HostId) -> bool {
    if !self.owes_confirmation(departed, successor) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    let Some(retirement) = self.retired.get_mut(&departed) else {
      return false;
    };
    let position = retirement
      .confirmed
      .partition_point(|host| host.0 < successor.0);
    retirement.confirmed.insert(position, successor);
    self.version = version;
    self.follow_moved_takeover(departed, successor, version);
    self.complete_retirements();
    self.bound_completed();
    true
  }

  /// Assigns `object`'s adoption to `adoption.successor` (`docs/wip/mirroring.md` decision 4): only to a member, and
  /// only where it changes what is assigned. Returns whether the configuration changed.
  pub fn assign_adoption(&mut self, object: ObjectId, adoption: Adoption) -> bool {
    if !self.assigns(object, &adoption) {
      return false;
    }
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.adoptions.insert(object, adoption);
    self.version = version;
    true
  }

  /// Whether assigning `adoption` to `object` would change the configuration: its successor is a member, and the
  /// object is not already assigned exactly so.
  pub fn assigns(&self, object: ObjectId, adoption: &Adoption) -> bool {
    self.members.contains(&adoption.successor) && self.adoptions.get(&object) != Some(adoption)
  }

  /// Drops `object`'s adoption once its successor reports it serves the object. Only the assigned successor's report
  /// changes anything. Returns whether the configuration changed.
  pub fn adopted(&mut self, object: ObjectId, successor: HostId) -> bool {
    if !self.owes_adopted(object, successor) {
      return false;
    }
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.adoptions.remove(&object);
    self.version = version;
    true
  }

  /// Whether `object` is assigned to `successor` and not yet reported adopted.
  pub fn owes_adopted(&self, object: ObjectId, successor: HostId) -> bool {
    self
      .adoptions
      .get(&object)
      .is_some_and(|adoption| adoption.successor == successor)
  }

  /// Where `object`'s line starts (§4.8 "Lookup"): its moved-object exception's owner, else its creator. Every register
  /// class of one object follows the same exception ([`ObjectId::placement_key`]), so a volume's catalog moves with it.
  fn line_start(&self, object: ObjectId) -> HostId {
    self
      .moved
      .get(&object.placement_key())
      .map_or_else(|| object.creator(), |moved| moved.owner)
  }

  /// The member that owns `object` now, from the configuration alone (§4.8 "Lookup": "a volume id carries its creator
  /// host … an ownership or home move is a configuration exception naming the affected volume and its authority
  /// generation"). Its line starts at its moved-object exception's owner, else at its creator; a member there owns it,
  /// and a retired one hands it on along the takeover's lineage ([`lineage`](RegionalConfiguration::lineage)). `None`
  /// when the line names no member: lost, settled past every kept retirement (the configuration no longer names the
  /// successor that adopted it; the object's holders do), or unresolved.
  pub fn owner_of(&self, object: ObjectId) -> Option<HostId> {
    let start = self.line_start(object);
    if self.members.contains(&start) {
      return Some(start);
    }
    match self.lineage(start, object) {
      Lineage::Successor { successor, .. } => Some(successor),
      Lineage::Lost | Lineage::Settled { .. } | Lineage::Unresolved => None,
    }
  }

  /// Whether `from` may move `object` to `to` (§4.10 "Ownership follows the writer": the planned handoff an operator's
  /// explicit move runs): `from` and `to` are two members, and the configuration names `from` as the object's owner
  /// ([`owner_of`](RegionalConfiguration::owner_of)) or names no owner because the object's line runs through a
  /// takeover that completed or one it keeps no record of. In those cases the configuration holds no claim against
  /// `from`: the successor that adopted the object is named only by the object's own records, and a member proposes a
  /// move only of a volume it serves under confirmed authority, which only that adopter does, while a host that lost its
  /// authority is retired and never proposes again under its id ([`admits`](RegionalConfiguration::admits)). An
  /// unresolved line is one of them since A-309: completion records are bounded, so the line of every volume whose
  /// creator's takeover completed long ago reads unresolved once its record is let go, and refusing it would leave such
  /// a volume unmovable for good. Refused for a lost line, which no member can have adopted (A-272), and for a line a
  /// kept retirement still carries to a successor other than `from`.
  pub fn moves(&self, object: ObjectId, from: HostId, to: HostId) -> bool {
    if from == to || !self.members.contains(&from) || !self.members.contains(&to) {
      return false;
    }
    let start = self.line_start(object);
    if self.members.contains(&start) {
      return start == from;
    }
    match self.lineage(start, object) {
      Lineage::Successor { successor, .. } => successor == from,
      Lineage::Settled { .. } | Lineage::Unresolved => true,
      Lineage::Lost => false,
    }
  }

  /// Moves `object` from its owner `from` to the member `to`, as [`moves`](RegionalConfiguration::moves) allows, its
  /// records placed at `f + 1` of `cohort`: its exception names `to` at the new version, its ownership generation,
  /// `from`, the owner the move took it from, and `cohort`, where `to`'s phase one finds the records. A move back to the
  /// creator keeps an exception too (A-291): the creator released its copy when the object left, and adopts it back by
  /// the same round any member does, which an exception names (an object absent from the exceptions is its creator's
  /// without one). Refused with an empty cohort or one without `from` (the owner is among its own candidates).
  /// Advances the version. Returns whether the configuration changed.
  pub fn move_object(
    &mut self,
    object: ObjectId,
    from: HostId,
    to: HostId,
    cohort: &[HostId],
  ) -> bool {
    if !self.moves(object, from, to) || !cohort.contains(&from) {
      return false;
    }
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    let mut cohort = cohort.to_vec();
    cohort.sort_unstable_by_key(|host| host.0);
    cohort.dedup();
    self.moved.insert(
      object.placement_key(),
      Moved {
        owner: to,
        version,
        from: Some(from),
        cohort,
      },
    );
    self.version = version;
    true
  }

  /// Whether `owner` may report `object`'s move adopted: the object's exception names `owner` and still names the
  /// member the move took it from (A-291).
  pub fn adopts_move(&self, object: ObjectId, owner: HostId) -> bool {
    self
      .moved
      .get(&object.placement_key())
      .is_some_and(|moved| moved.owner == owner && moved.from.is_some())
  }

  /// Records `object`'s move adopted by its new owner `owner` (A-291): the exception keeps its owner and drops the
  /// member it left and the move's cohort, which is that member's word to let its copy go, and no round is owed any
  /// more. The ownership generation is unchanged. Advances the version. Returns whether the configuration changed.
  pub fn adopt_move(&mut self, object: ObjectId, owner: HostId) -> bool {
    if !self.adopts_move(object, owner) {
      return false;
    }
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    if let Some(moved) = self.moved.get_mut(&object.placement_key()) {
      moved.from = None;
      moved.cohort = Vec::new();
    }
    self.version = version;
    true
  }

  /// Whether `owner` may drop `object`'s exception: the object is moved, and `owner` owns it now.
  pub fn forgets_moved(&self, object: ObjectId, owner: HostId) -> bool {
    self.moved.contains_key(&object.placement_key()) && self.owner_of(object) == Some(owner)
  }

  /// Drops `object`'s exception at its owner's report that the object is destroyed, so an exception never outlives
  /// its object. Returns whether the configuration changed.
  pub fn forget_moved(&mut self, object: ObjectId, owner: HostId) -> bool {
    if !self.forgets_moved(object, owner) {
      return false;
    }
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.moved.remove(&object.placement_key());
    self.version = version;
    true
  }

  /// Follows a takeover in the moved-object exceptions as `successor` confirms its share of `departed`'s: every
  /// exception whose line the takeover's lineage carries to `successor` in `departed`'s round
  /// ([`Lineage::Successor`]) names `successor` from `version` on, or is dropped when `successor` is the object's
  /// creator, whose line it is back on. A line is followed whether it starts at `departed` or at a host whose own
  /// successor retired before confirming and carried it into `departed`'s takeover. Done before the retirement can be
  /// pruned, so no exception's line ever starts at a retirement the configuration no longer keeps.
  fn follow_moved_takeover(&mut self, departed: HostId, successor: HostId, version: u64) {
    let followed: Vec<ObjectId> = self
      .moved
      .iter()
      .filter(|(object, moved)| {
        !self.members.contains(&moved.owner)
          && self.lineage(moved.owner, **object)
            == Lineage::Successor {
              departed,
              successor,
            }
      })
      .map(|(object, _)| *object)
      .collect();
    for object in followed {
      if successor == object.creator() {
        self.moved.remove(&object);
      } else {
        self.moved.insert(
          object,
          Moved {
            owner: successor,
            version,
            from: None,
            cohort: Vec::new(),
          },
        );
      }
    }
  }

  /// Whether `host` still owes a confirmation of its share of `departed`'s takeover: `departed`'s retirement is
  /// kept, `host` was one of its survivors ([`Retirement::survivors`]) and is still a member, and `host` has not
  /// confirmed yet. A survivor that retired before confirming owes nothing here: its own retirement carries
  /// the takeover ([`Retirement::unconfirmed`]).
  pub fn owes_confirmation(&self, departed: HostId, host: HostId) -> bool {
    self.members.contains(&host)
      && self.retired.get(&departed).is_some_and(|retirement| {
        retirement.survivors.contains(&host) && !retirement.confirmed.contains(&host)
      })
  }

  /// The survivors of `departed`'s retirement ([`Retirement::survivors`]), or none when it is not kept.
  pub fn survivors_of(&self, departed: HostId) -> Vec<HostId> {
    self
      .retired
      .get(&departed)
      .map(|retirement| retirement.survivors.clone())
      .unwrap_or_default()
  }

  /// Whether `member` may be admitted: it is not a member, and no kept retirement names it — as the retired
  /// host, or as one of a retirement's survivors. A host a kept retirement names is ranked in that takeover's
  /// lineage ([`lineage`](RegionalConfiguration::lineage)); admitting it again under the same id before every
  /// such takeover is done would let a later node read the returning host as the successor that never left.
  /// It is admitted once those retirements are dropped (a restart rejoins at once, under a fresh id).
  pub fn admits(&self, member: HostId) -> bool {
    !self.members.contains(&member)
      && !self
        .retired
        .iter()
        .any(|(departed, retirement)| *departed == member || retirement.survivors.contains(&member))
  }

  /// The retired owners whose settled cohorts `departed`'s objects are recovered through: `departed` itself
  /// and, transitively, every retired host whose takeover it had not confirmed its share of when it retired —
  /// an object it was taking over from one of them may not have been re-committed under it, so it is still
  /// held only by that host's cohort. In id order; empty if `departed` has no retirement kept.
  pub fn recovery_owners(&self, departed: HostId) -> Vec<HostId> {
    let mut owners: Vec<HostId> = Vec::new();
    let mut frontier = vec![departed];
    while let Some(owner) = frontier.pop() {
      if owners.contains(&owner) {
        continue;
      }
      let Some(retirement) = self.retired.get(&owner) else {
        continue;
      };
      owners.push(owner);
      frontier.extend(retirement.unconfirmed.iter().copied());
    }
    owners.sort_unstable_by_key(|host| host.0);
    owners
  }

  /// Every host of the neighbourhoods `departed`'s objects are recovered through
  /// ([`recovery_owners`](RegionalConfiguration::recovery_owners)), settled and, where a change was in flight,
  /// current ([`Retirement::current`], A-309), retired ones included, in id order: the hosts that may hold a record of
  /// them, and among whose members a takeover of them runs its round.
  pub fn recovery_hosts(&self, departed: HostId) -> Vec<HostId> {
    let mut hosts: Vec<HostId> = self
      .recovery_owners(departed)
      .iter()
      .filter_map(|owner| self.retired.get(owner))
      .flat_map(|retirement| {
        retirement
          .settled
          .hosts
          .iter()
          .chain(
            retirement
              .current
              .iter()
              .flat_map(|current| current.hosts.iter()),
          )
          .copied()
      })
      .collect();
    hosts.sort_unstable_by_key(|host| host.0);
    hosts.dedup();
    hosts
  }

  /// The cohorts `object` of the retired `departed` is recovered through: for each of its recovery owners, the
  /// cohort that owner placed `object` on in its settled neighbourhood. A takeover adopts only once `f + 1`
  /// survivors of **each** have promised, so it meets every record any of them committed. Empty if `departed`
  /// has no retirement kept.
  pub fn recovery_cohorts(&self, departed: HostId, object: ObjectId) -> Vec<Vec<HostId>> {
    self
      .recovery_owners(departed)
      .iter()
      .filter_map(|owner| {
        self
          .retired
          .get(owner)
          .map(|retirement| retirement.settled.cohort(*owner, object, self.quorum))
      })
      .collect()
  }

  /// The cohorts `object` of the retired `departed` was placed on in its recovery owners' current neighbourhoods, where
  /// a change was in flight when they retired ([`Retirement::current`], A-309): the hosts its joint writes and
  /// re-shipped heads reached besides the settled cohorts ([`recovery_cohorts`]). A takeover counts them only past
  /// tolerance, when a settled cohort has fewer members than its recovery quorum (`slates_db::takeover::readiness`).
  /// Empty when no recovery owner retired with a change in flight.
  ///
  /// [`recovery_cohorts`]: RegionalConfiguration::recovery_cohorts
  pub fn current_cohorts(&self, departed: HostId, object: ObjectId) -> Vec<Vec<HostId>> {
    self
      .recovery_owners(departed)
      .iter()
      .filter_map(|owner| {
        self
          .retired
          .get(owner)
          .and_then(|retirement| retirement.current.as_ref())
          .map(|current| current.cohort(*owner, object, self.quorum))
      })
      .collect()
  }

  /// The member that takes `object` of the retired `departed` over: the one rendezvous ranks first among the
  /// survivors of its recovery cohorts ([`recovery_cohorts`](RegionalConfiguration::recovery_cohorts)), or, when no
  /// survivor is in any of them, among all the retirement's survivors (A-309): every object then ranks one, so the
  /// survivors' confirmations cover every object some of them hold and a completed takeover means each was adopted.
  /// Before, such an object ranked no one: no round learned it, every survivor confirmed, and the completed takeover
  /// read its line settled while a member of the current neighbourhood still held its copy. The survivors are fixed at
  /// the retirement, so every node names the same one from the committed configuration alone, never re-ranked. `None`
  /// if the retirement has no survivor, or `departed` has no retirement kept.
  pub fn successor(&self, departed: HostId, object: ObjectId) -> Option<HostId> {
    let retirement = self.retired.get(&departed)?;
    let mut ranked: Vec<HostId> = self
      .recovery_cohorts(departed, object)
      .into_iter()
      .flatten()
      .filter(|host| retirement.survivors.contains(host))
      .collect();
    if ranked.is_empty() {
      ranked.clone_from(&retirement.survivors);
    }
    ranked.sort_unstable_by_key(|host| host.0);
    ranked.dedup();
    rendezvous_first(&ranked, object)
  }

  /// Where `object`, last owned by the retired `owner`, belongs now (§4.8 "Promotion and takeover"), from the
  /// configuration alone. The walk follows successors: a successor that has itself retired while its
  /// retirement is kept carries the object into its own takeover (it may not have re-committed the object
  /// yet, and if it did, the object is its own); one that is a member is where the object belongs. Each step
  /// moves to a host that was a member when the previous one retired, so the walk ends within the kept
  /// retirements, and two nodes that installed the same configuration by different paths resolve alike. A walk that
  /// leaves the kept retirements ends settled only at a completion record, and unresolved anywhere else
  /// ([`line_end`](RegionalConfiguration::line_end)).
  pub fn lineage(&self, owner: HostId, object: ObjectId) -> Lineage {
    let mut departed = owner;
    for _ in 0..=self.retired.len() {
      if !self.retired.contains_key(&departed) {
        return self.line_end(departed);
      }
      let Some(successor) = self.successor(departed, object) else {
        return Lineage::Lost;
      };
      if self.retired.contains_key(&successor) {
        departed = successor;
      } else if self.members.contains(&successor) {
        return Lineage::Successor {
          departed,
          successor,
        };
      } else {
        // A ranked successor neither a member nor a kept retirement: settled only if its own takeover, which took the
        // object over or carried this one, completed (A-272, A-309).
        return self.line_end(successor);
      }
    }
    Lineage::Unresolved
  }

  /// How a line ends at `host`, a host no kept retirement is of and no member: [`Lineage::Settled`] on its takeover's
  /// completion record, the only evidence its objects were adopted (A-309), and [`Lineage::Unresolved`] without one:
  /// a retirement the bound dropped, a completion record let go, or a host the configuration never retired. Before
  /// A-309 every absence read settled but a recorded drop (A-272), so a takeover that left no record, such as one with
  /// no survivor, settled lines nobody adopted.
  fn line_end(&self, host: HostId) -> Lineage {
    if self.completed.contains_key(&host) {
      Lineage::Settled { takeover: host }
    } else {
      Lineage::Unresolved
    }
  }

  /// Whether `holder`'s copy of an object whose line is [`Lineage::Settled`] at `takeover` is stale (A-309): `holder`
  /// confirmed its own share of that completed takeover, so it was a member of its recovery neighbourhoods throughout its
  /// round and answered every successor in full before any confirmed, and every copy it held that fell to a successor
  /// was learned and adopted. A holder outside those neighbourhoods was never asked: its copy may be the only one of an
  /// object no successor learned (a neighbourhood the owner wrote to before it settled elsewhere), so it keeps it.
  pub fn settled_for(&self, takeover: HostId, holder: HostId) -> bool {
    self
      .completed
      .get(&takeover)
      .is_some_and(|record| record.confirmed.contains(&holder))
  }

  /// Derived: the most retirements the configuration keeps — one per remaining member. A retirement is kept
  /// only until its survivors confirm their shares, which each does once its round and adoptions finish. With
  /// `k` retirements outstanding in a region formed at `n ≥ 2f + 1` members, `n − k` remain, and `k ≤ n − k`
  /// holds for every `k ≤ f`: a region never drops a takeover it can still complete while it has lost no more
  /// hosts than it tolerates. Past that one no survivor owes anything of goes first, then the oldest. This bounds the
  /// map at the order of the per-member neighbourhoods and domains while every restart retires an id (banned item 8),
  /// and never below one, so the newest retirement is always kept.
  pub fn retirement_bound(&self) -> usize {
    self.members.len().max(1)
  }

  /// Derived: the completion records the configuration keeps besides those a kept retirement still needs (one per
  /// remaining member, as for the retirements, [`retirement_bound`](RegionalConfiguration::retirement_bound)). A record
  /// is read by a holder that still holds a copy of the retired host's objects at its next install: every member that
  /// held one when the takeover completed lets it go at that install, so only a holder that installs nothing while
  /// `members` more takeovers complete still needs it, and that holder keeps its stale copy (a leak bounded by what it
  /// holds, never a loss: a line past a record let go reads unresolved). A record a kept retirement still needs, to know
  /// that a survivor which left owing its share carried it into a takeover that completed, is never let go
  /// ([`bound_completed`](RegionalConfiguration::bound_completed)). This keeps the configuration's growth in the order of
  /// its members while every restart retires an id (banned item 8).
  pub fn completed_bound(&self) -> usize {
    self.members.len().max(1)
  }

  /// Whether some member still owes a confirmation of `departed`'s takeover ([`owes_confirmation`]).
  ///
  /// [`owes_confirmation`]: RegionalConfiguration::owes_confirmation
  fn owed(&self, departed: HostId) -> bool {
    self.retired.get(&departed).is_some_and(|retirement| {
      retirement
        .survivors
        .iter()
        .any(|survivor| self.owes_confirmation(departed, *survivor))
    })
  }

  /// Whether a kept retirement recovers through `departed`'s: one whose host left owing a share of `departed`'s
  /// takeover and so carries it ([`Retirement::unconfirmed`]).
  fn carried(&self, departed: HostId) -> bool {
    self
      .retired
      .values()
      .any(|retirement| retirement.unconfirmed.contains(&departed))
  }

  /// Whether `departed`'s kept retirement is done and complete (A-309), so it becomes a completion record: no member
  /// owes a confirmation of it, no kept retirement still recovers through it, it had survivors, and each of them
  /// confirmed its share or left owing it and carried it into a takeover of its own that completed. A survivor that left
  /// owing its share retired while this retirement was kept, so its own retirement carried this one; that takeover's
  /// completion is the evidence its share was adopted. A retirement with no survivor completes never: nobody adopted
  /// its objects (before A-309 it was dropped at once, and its lines read settled). Nor does one whose survivor left and
  /// left no completion behind (its own retirement dropped by the bound, or with no survivor either).
  pub fn completes(&self, departed: HostId) -> bool {
    let Some(retirement) = self.retired.get(&departed) else {
      return false;
    };
    !retirement.survivors.is_empty()
      && !self.owed(departed)
      && !self.carried(departed)
      && retirement.survivors.iter().all(|survivor| {
        retirement.confirmed.contains(survivor) || self.completed.contains_key(survivor)
      })
  }

  /// Turns every retirement whose takeover completed into its completion record (A-309): it leaves the kept
  /// retirements, and its lines read settled from then on. Repeated to a fixed point, since one completing can complete
  /// another it carried. A retirement no member owes anything of that does not complete stays kept, so its lines still
  /// name the successors that adopted and the ones nobody did; [`bound_retirements`] lets those go first.
  ///
  /// [`bound_retirements`]: RegionalConfiguration::bound_retirements
  fn complete_retirements(&mut self) {
    loop {
      let done = self
        .retired
        .keys()
        .copied()
        .find(|departed| self.completes(*departed));
      let Some(departed) = done else {
        break;
      };
      let Some(retirement) = self.retired.remove(&departed) else {
        break;
      };
      self.completed.insert(
        departed,
        CompletedTakeover {
          version: self.version,
          confirmed: retirement.confirmed,
        },
      );
    }
  }

  /// Keeps at most [`retirement_bound`](RegionalConfiguration::retirement_bound) retirements. Beyond it, one that no
  /// member owes a confirmation of and no kept retirement recovers through goes first (it can never complete, A-309),
  /// then the oldest (by the version that retired it, the id breaking a tie). A takeover dropped this way leaves no
  /// record: its lines read unresolved, never settled, and its objects' holders keep their copies.
  ///
  /// Every kept retirement that recovers through the one dropped goes with it (A-309). A takeover's successors are
  /// ranked among its recovery cohorts, those of the takeovers it carries among them ([`successor`]), so a takeover kept
  /// past one it carried would rank its objects anew: a survivor routed a copy to the successor first ranked, that
  /// successor confirmed its share without the object (it ranked another by then), and a holder outside its placement
  /// read the confirmation as the object's adoption and let the copy go (`crates/db/tests/lineage_model.rs` found it).
  /// Dropped together, every kept takeover ranks as it did at its retirement.
  ///
  /// [`successor`]: RegionalConfiguration::successor
  fn bound_retirements(&mut self) {
    while self.retired.len() > self.retirement_bound() {
      let first = self
        .retired
        .iter()
        .min_by_key(|(host, retirement)| {
          let pending = self.owed(**host) || self.carried(**host);
          (pending, retirement.version, host.0)
        })
        .map(|(host, _)| *host);
      let Some(host) = first else {
        break;
      };
      let dropped: Vec<HostId> = self
        .retired
        .keys()
        .copied()
        .filter(|departed| self.recovery_owners(*departed).contains(&host))
        .collect();
      for departed in dropped {
        self.retired.remove(&departed);
      }
    }
  }

  /// Keeps at most [`completed_bound`](RegionalConfiguration::completed_bound) completion records, or the ones kept
  /// retirements still need when those are more: the record of a survivor that left owing its share, whose completed
  /// takeover carried that share ([`completes`](RegionalConfiguration::completes)), is never let go while that
  /// retirement is kept. Beyond the bound the oldest other record (by the version it completed at, the id breaking a
  /// tie) goes first.
  fn bound_completed(&mut self) {
    let needed: std::collections::BTreeSet<HostId> = self
      .retired
      .values()
      .flat_map(|retirement| {
        retirement
          .survivors
          .iter()
          .copied()
          .filter(|survivor| !retirement.confirmed.contains(survivor))
      })
      .collect();
    let mut evictable: Vec<(u64, HostId)> = self
      .completed
      .iter()
      .filter(|(host, _)| !needed.contains(host))
      .map(|(host, record)| (record.version, *host))
      .collect();
    let excess = self.completed.len().saturating_sub(self.completed_bound());
    evictable.sort_unstable_by_key(|(version, host)| (*version, host.0));
    for (_, host) in evictable.into_iter().take(excess) {
      self.completed.remove(&host);
    }
  }

  /// Takes over a dead host (§4.8): bumps its fencing epoch (so its in-flight records are refused as
  /// `StaleEpoch`), retires it, and refixes the neighbourhoods. The dead host's objects are not stored here:
  /// its [`Retirement`] names the cohorts they are recovered through, and every node ranks each object's
  /// successor from it ([`successor`](RegionalConfiguration::successor)). Returns the host's new epoch, or
  /// `None` — nothing changed — when its epoch or the version has no successor: a repeated epoch would let the
  /// dead host's records pass its fence (AUD-29-26's sibling).
  pub fn take_over(&mut self, dead: HostId, scatter: u64) -> Option<HostEpoch> {
    let current = self.epochs.get(&dead).copied().unwrap_or(FIRST_EPOCH);
    let bumped = HostEpoch(current.0.checked_add(1)?);
    self.version.checked_add(1)?;
    self.epochs.insert(dead, bumped);
    self.retire(dead, scatter);
    Some(bumped)
  }
}

/// The root configuration — the cross-region state the **root group** agrees on (§4.8, D-14: "a root group
/// across regions holds region membership and moved homes"; line 1690 "a root group across regions holds
/// region membership and cross-region promotions"). It is the region-level counterpart of a
/// [`RegionalConfiguration`]: where the regional configuration holds a region's host membership, the root
/// configuration holds the *regions* themselves, the volumes whose home region has moved, and the region each
/// lost region has been promoted to. Written **only** on a region-membership change, a home move, or a
/// region-loss promotion — never per write (banned item 10), a near-zero rate the design makes a tripwire. On
/// a laptop it holds the sole region, so the root group is the degenerate self-leading group (R8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootConfiguration {
  /// Advances on every region-membership, home or promotion change, so a request under a stale root version
  /// is refused exactly as a regional one is.
  pub version: u64,
  /// The regions the root group agrees exist, in id order.
  pub regions: Vec<RegionId>,
  /// The volumes whose home region has **moved** away from their creator region (moved volumes only, §4.8
  /// line 1616 `homes: Art<VolumeId, Region>`); a volume absent here is homed in its creator region. Keyed by
  /// the volume's object id (a volume id carries its creator host, §4.8 "Lookup").
  pub homes: std::collections::BTreeMap<ObjectId, RegionId>,
  /// Each **lost** region and the region promoted to serve it (§4.8 "region loss promotes the mirror through
  /// the root group"): a volume homed in a lost region is served from its promotion, recomputed at lookup
  /// (per-region, like a per-host takeover — no global catalog, D-12). A region present here is no longer in
  /// `regions`.
  pub promotions: std::collections::BTreeMap<RegionId, RegionId>,
}

impl RootConfiguration {
  /// The formed root configuration over `regions` (version 0, no moved homes, no promotions) — on a laptop,
  /// the sole region. Regions are sorted and de-duplicated so two nodes form an identical configuration.
  pub fn formed(regions: Vec<RegionId>) -> RootConfiguration {
    let mut regions = regions;
    regions.sort_unstable_by_key(|region| region.0);
    regions.dedup();
    RootConfiguration {
      version: 0,
      regions,
      homes: std::collections::BTreeMap::new(),
      promotions: std::collections::BTreeMap::new(),
    }
  }

  /// Admits `region` to the root membership (a region joins the fleet). Returns whether it changed — an
  /// already-present region is a no-op, so the root log grows only for real changes.
  pub fn admit_region(&mut self, region: RegionId) -> bool {
    if self.regions.contains(&region) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.regions.push(region);
    self.regions.sort_unstable_by_key(|region| region.0);
    self.version = version;
    true
  }

  /// Retires `region` cleanly from the root membership (an operator removal, not a loss — no promotion, since
  /// nothing must be served from a mirror). Returns whether it changed.
  pub fn retire_region(&mut self, region: RegionId) -> bool {
    if !self.regions.contains(&region) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.regions.retain(|r| *r != region);
    self.version = version;
    true
  }

  /// Promotes `mirror` to serve a **lost** `region` (§4.8 "region loss promotes the mirror through the root
  /// group"): records the promotion and drops the lost region from the membership, so a volume homed in the
  /// lost region is thereafter served from `mirror` ([`home_of`](RootConfiguration::home_of)). Returns whether
  /// it changed — a no-op if `region` or `mirror` is not a member, or the two are the same (a region cannot be
  /// promoted to itself).
  pub fn promote_region(&mut self, region: RegionId, mirror: RegionId) -> bool {
    if region == mirror || !self.regions.contains(&region) || !self.regions.contains(&mirror) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.regions.retain(|r| *r != region);
    self.promotions.insert(region, mirror);
    self.version = version;
    true
  }

  /// Moves `volume`'s home to region `to` (§4.8 "the homes of moved volumes"; "a region-home move also carries
  /// root-group authority"). Returns whether it changed — a no-op if `to` is not a member or the volume is
  /// already homed there.
  pub fn move_home(&mut self, volume: ObjectId, to: RegionId) -> bool {
    if !self.regions.contains(&to) || self.homes.get(&volume) == Some(&to) {
      return false;
    }
    // A version with no successor refuses the change before anything moves, identically on every replica
    // that applies it (AUD-29-26's sibling: a repeated version would name two configurations).
    let Some(version) = self.version.checked_add(1) else {
      return false;
    };
    self.homes.insert(volume, to);
    self.version = version;
    true
  }

  /// The region a `volume` is served from, given its `creator_region` (the region of the host that created
  /// it — the volume id carries the creator host, §4.8 "Lookup", and the caller maps that host to its region).
  /// It is the volume's explicitly moved home if it has one, otherwise its creator region; then any promotion
  /// of that region is followed (a lost region's volumes route to its successor), to a fixed point bounded by
  /// the region count so a malformed promotion cycle cannot loop. This is the root-group analogue of
  /// [`configuration_for`](RegionalConfiguration::configuration_for): it names *where* a volume lives, where
  /// the regional configuration names *who* owns an object.
  pub fn home_of(&self, volume: ObjectId, creator_region: RegionId) -> RegionId {
    let mut region = self.homes.get(&volume).copied().unwrap_or(creator_region);
    // Follow promotions to a fixed point; the promotion count bounds the chain, so even a malformed cycle
    // terminates rather than spinning.
    for _ in 0..self.promotions.len() {
      match self.promotions.get(&region) {
        Some(&next) if next != region => region = next,
        _ => break,
      }
    }
    region
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// §4.8 "joint writes" (the bug record of 2026-11-06). A bootstrap settles the first node's cohort as itself
  /// alone, so while its neighbourhood grows to the region a record's placement joins that one-member cohort with the
  /// new one. At `f = 1` a cohort of one can never give `f + 1` acknowledgements, so no record of an object placed in
  /// that window ever committed. Do: judge the joint placement `[[a], [a, b, c]]` at `f = 1` against acknowledgement
  /// sets. Expect it placed by `{a, b, c}` and `{a, b}` (all of the old cohort, `f + 1` of the new), and not by `{a}`
  /// (one of the new) or `{b, c}` (none of the old).
  #[test]
  fn a_joint_placement_with_a_cohort_smaller_than_the_quorum_needs_every_member_of_it() {
    let (a, b, c) = (HostId(1), HostId(2), HostId(3));
    let placement = Placement {
      candidates: vec![a, b, c],
      acked: Vec::new(),
      mirror_acked: None,
      joint: vec![vec![a], vec![a, b, c]],
    };
    let quorum = Quorum { f: 1 };
    assert!(
      placement.placed_with(&[a, b, c], quorum),
      "every candidate acknowledged"
    );
    assert!(
      placement.placed_with(&[a, b], quorum),
      "all of the old cohort and f + 1 of the new"
    );
    assert!(
      !placement.placed_with(&[a], quorum),
      "one of the new cohort"
    );
    assert!(
      !placement.placed_with(&[b, c], quorum),
      "none of the old cohort"
    );
  }

  /// docs/wip/mirroring.md decision 4. Do: assign a promoted object's adoption to a member and to a non-member, report
  /// it adopted by another member, then by its successor. Expect: only the member's assignment and only its own
  /// report change the configuration, each advancing the version, and the adopted entry is gone.
  #[test]
  fn an_adoption_is_assigned_to_a_member_and_dropped_only_by_its_successor() {
    let (a, b, c) = (HostId(1), HostId(2), HostId(3));
    let mut config =
      RegionalConfiguration::formed(vec![a, b, c], Quorum { f: 1 }, Default::default(), 3, true);
    let object = ObjectId::new(HostId(90), 1);
    let adoption = |successor| Adoption {
      successor,
      head: vec![7],
      catalog: vec![8],
    };
    assert!(
      !config.assign_adoption(object, adoption(HostId(99))),
      "a non-member is assigned nothing"
    );
    let version = config.version;
    assert!(config.assign_adoption(object, adoption(b)));
    assert_eq!(config.version, version + 1);
    assert!(
      !config.assign_adoption(object, adoption(b)),
      "the same assignment changes nothing"
    );
    assert!(
      !config.adopted(object, c),
      "another member's report changes nothing"
    );
    assert!(config.adopted(object, b));
    assert!(config.adoptions.is_empty());
    assert_eq!(config.version, version + 2);
  }

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
      &config.domains,
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

  /// AUD-29-43: do: accept three sequences of one single-value register and one of another, compact the first
  /// below its newest, then forget the second; expect phase one to report the first's newest record unchanged,
  /// the older positions gone from the durable state, the other object untouched by the compaction and gone
  /// after its forget.
  #[test]
  fn a_compacted_register_keeps_its_newest_position_and_a_forgotten_one_keeps_nothing() {
    let config = Configuration::solo(HostId(1));
    let authority = Authority {
      generation: config.version,
      owner: config.owner,
    };
    let mut acceptor = Acceptor::new(config.owner, authority);
    for sequence in 1..=3 {
      acceptor
        .accept(&record_under(
          &config,
          7,
          sequence,
          HostEpoch(1),
          &sequence.to_le_bytes(),
        ))
        .unwrap();
    }
    acceptor
      .accept(&record_under(&config, 8, 1, HostEpoch(1), b"other"))
      .unwrap();
    let first = ObjectId::new(config.owner, 7);
    let second = ObjectId::new(config.owner, 8);
    acceptor.compact_below(first, 3);
    let positions = |acceptor: &Acceptor, object: ObjectId| -> Vec<u64> {
      acceptor
        .persisted()
        .1
        .into_iter()
        .filter(|(held, ..)| *held == object)
        .map(|(_, sequence, ..)| sequence)
        .collect()
    };
    assert_eq!(positions(&acceptor, first), vec![3]);
    assert_eq!(positions(&acceptor, second), vec![1]);
    let prepare = |object| Prepare {
      owner: config.owner,
      object,
      epoch: HostEpoch(2),
      generation: config.version,
    };
    let promise = acceptor.prepare(&prepare(first)).unwrap();
    assert_eq!(
      promise.highest.map(|held| (held.sequence, held.value)),
      Some((3, 3u64.to_le_bytes().to_vec()))
    );
    acceptor.forget_object(second);
    assert!(positions(&acceptor, second).is_empty());
    assert_eq!(acceptor.prepare(&prepare(second)).unwrap().highest, None);
  }

  /// §4.8 "persistence before reply", A-367: a holder's acceptor that tracks its changes names exactly the positions a
  /// publication must put and remove, and one that does not names none. Do: on a tracking acceptor, accept positions 1
  /// to 3 of a ledger and 1 of another object, clear; accept position 4, accept position 2 again at a newer epoch, compact
  /// the other object below 2 after accepting its 2 (at the newer epoch, the fence being the acceptor's), then forget the
  /// ledger and accept its 5; on an acceptor made without
  /// tracking, accept a position. Expect after the clear nothing changed; then written {ledger 5, other 2} and dropped
  /// {ledger 1 to 4, other 1}, with the epoch of a written position readable by `position`; the untracked acceptor's
  /// changes `None`; an acceptor that holds a position when it begins to track counts it written, and a clear empties it.
  #[test]
  fn a_tracking_acceptor_names_every_position_written_or_dropped_since_its_last_clear() {
    let config = Configuration::solo(HostId(1));
    let authority = Authority {
      generation: config.version,
      owner: config.owner,
    };
    let mut acceptor = Acceptor::new(config.owner, authority).tracking_changes();
    let accept = |acceptor: &mut Acceptor, object: u64, sequence: u64, epoch: u64| {
      acceptor
        .accept(&record_under(
          &config,
          object,
          sequence,
          HostEpoch(epoch),
          &sequence.to_le_bytes(),
        ))
        .unwrap();
    };
    for sequence in 1..=3 {
      accept(&mut acceptor, 7, sequence, 1);
    }
    accept(&mut acceptor, 8, 1, 1);
    acceptor.clear_changes();
    let cleared = acceptor.changes().cloned();
    let (ledger, other) = (
      ObjectId::new(config.owner, 7),
      ObjectId::new(config.owner, 8),
    );
    accept(&mut acceptor, 7, 4, 1);
    accept(&mut acceptor, 7, 2, 2);
    accept(&mut acceptor, 8, 2, 2);
    acceptor.compact_below(other, 2);
    acceptor.forget_object(ledger);
    accept(&mut acceptor, 7, 5, 2);
    let changes = acceptor.changes().cloned().unwrap();
    let mut untracked = Acceptor::new(config.owner, authority);
    accept(&mut untracked, 7, 1, 1);
    let mut tracked_late = Acceptor::new(config.owner, authority);
    accept(&mut tracked_late, 9, 1, 1);
    let mut tracked_late = tracked_late.tracking_changes();
    let late_written = tracked_late
      .changes()
      .map(|changes| changes.written.clone());
    tracked_late.clear_changes();
    assert_eq!(
      cleared,
      Some(PositionChanges::default()),
      "nothing changed since the clear"
    );
    assert_eq!(
      changes.written.into_iter().collect::<Vec<_>>(),
      vec![(ledger, 5), (other, 2)],
      "the positions accepted and still held"
    );
    assert_eq!(
      changes.dropped.into_iter().collect::<Vec<_>>(),
      vec![
        (ledger, 1),
        (ledger, 2),
        (ledger, 3),
        (ledger, 4),
        (other, 1)
      ],
      "the positions compacted or forgotten and not accepted again"
    );
    assert_eq!(
      acceptor.position(ledger, 5).map(|(epoch, _)| epoch),
      Some(HostEpoch(2))
    );
    assert!(
      untracked.changes().is_none(),
      "an untracked acceptor keeps none"
    );
    assert_eq!(
      late_written,
      Some([(ObjectId::new(config.owner, 9), 1)].into_iter().collect()),
      "an acceptor tracked once it holds a position counts that position written"
    );
    assert_eq!(tracked_late.changes(), Some(&PositionChanges::default()));
  }

  /// Shape: the largest `f` the exhaustive quorum oracles enumerate — cohorts up to `2f + 1 = 7` hosts, so
  /// every subset of a cohort is a 7-bit mask.
  const ORACLE_MAX_F: u32 = 3;

  /// The subsets of `hosts` hosts (as bit masks) with exactly `size` members.
  fn subsets_of_size(hosts: usize, size: usize) -> impl Iterator<Item = u32> {
    (0..1u32 << hosts).filter(move |mask| mask.count_ones() as usize == size)
  }

  /// §4.8 "Promotion and takeover" (Flexible Paxos's `q1 + q2 > n`): for every `f` up to [`ORACLE_MAX_F`] and every
  /// cohort a region can fix — one host up to the `2f + 1` floor — a phase one of [`Quorum::recovery`] promises from
  /// the cohort's survivors (the retired owner, host 0, excluded) meets every `f + 1` commit the cohort could have
  /// made, the retired owner's own acknowledgement among it or not; and one promise fewer misses some commit
  /// wherever a smaller round could adopt at all. At the floor the rule is `f + 1`.
  #[test]
  fn a_recovery_quorum_meets_every_commit_its_cohort_could_make_and_no_smaller_one_does() {
    for f in 0..=ORACLE_MAX_F {
      let quorum = Quorum { f };
      for hosts in 1..=quorum.candidates() {
        let promises = quorum.recovery(hosts);
        let survivors = (1u32 << hosts) - 2;
        let commits: Vec<u32> = subsets_of_size(hosts, quorum.commit()).collect();
        let rounds: Vec<u32> = subsets_of_size(hosts, promises)
          .filter(|round| round & !survivors == 0)
          .collect();
        for commit in &commits {
          for round in &rounds {
            assert_ne!(
              commit & round,
              0,
              "f = {f}, {hosts} hosts: a round of {promises} meets every commit"
            );
          }
        }
        if promises > 1 {
          let missed = subsets_of_size(hosts, promises - 1)
            .filter(|round| round & !survivors == 0)
            .any(|round| commits.iter().any(|commit| commit & round == 0));
          assert!(
            missed,
            "f = {f}, {hosts} hosts: {} promises can miss a commit",
            promises - 1
          );
        }
      }
      assert_eq!(quorum.recovery(quorum.candidates()), quorum.commit());
    }
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
      // A floor neighbourhood at f=1: owner + 2f = one copyset of three, so every object has three
      // candidates (above the floor an object takes just its one copyset, tested separately).
      neighbourhood: vec![owner, HostId(2), HostId(3)],
      domains: std::collections::BTreeMap::new(),
      quorum: Quorum { f: 1 },
      has_mirror: false,
      settled: None,
      generation: 0,
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

  /// AC (§4.13/§4.8): a record from a principal the authority does not authorize is refused — there is no
  /// unauthenticated acceptance. And the configuration-generation gate is split by direction (§4.8 the
  /// piggyback rule): a record from an **older** configuration is [`ConfigurationStale`] naming the holder's
  /// current (newer) version so the sender refreshes; one from a **newer** configuration is
  /// [`ForeignGeneration`] naming the holder's current version (the holder is behind and refreshes reactively).
  #[test]
  fn authority_refuses_wrong_owner_and_splits_the_generation_gate_by_direction() {
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

    // A record from an older configuration (6 < 7): the sender is stale — refused with the holder's current
    // (newer) version, the value the sender refreshes toward and retries under.
    let older_configuration = Record {
      owner,
      object: ObjectId::new(HostId(1), 1),
      sequence: 0,
      epoch: HostEpoch(1),
      generation: 6,
      value: b"x".to_vec(),
    };
    assert_eq!(
      acceptor.accept(&older_configuration),
      Err(RegisterError::ConfigurationStale { version: 7 })
    );

    // A record from a newer configuration (8 > 7): this holder is behind — it cannot validate authority under
    // a generation it has not installed, so it refuses naming its own current generation (and refreshes).
    let newer_configuration = Record {
      owner,
      object: ObjectId::new(HostId(1), 1),
      sequence: 0,
      epoch: HostEpoch(1),
      generation: 8,
      value: b"x".to_vec(),
    };
    assert_eq!(
      acceptor.accept(&newer_configuration),
      Err(RegisterError::ForeignGeneration { current: 7 })
    );
  }

  /// AC (§4.8 the piggyback rule): a `ConfigurationStale` refusal rides back on the record wire carrying the
  /// holder's current version and decodes to exactly that; every other refusal, and an ack-shaped reply,
  /// carries nothing actionable (so a sender's `Ack::decode`-then-`decode_refusal` never confuses the two).
  #[test]
  fn a_configuration_stale_refusal_rides_back_and_other_replies_do_not() {
    // The one carried refusal round-trips its version.
    let wire = encode_refusal(&RegisterError::ConfigurationStale { version: 42 });
    assert_eq!(
      decode_refusal(&wire),
      Some(Refusal::ConfigurationStale { version: 42 })
    );

    // Other refusals carry nothing back (an empty reply the sender counts as no acknowledgement).
    for other in [
      RegisterError::Unauthorized,
      RegisterError::ConflictingPosition,
      RegisterError::StaleEpoch { current: 3 },
    ] {
      assert!(encode_refusal(&other).is_empty());
    }

    // An ack-shaped reply (72 bytes) and hostile input never decode as a refusal — length and tag disambiguate.
    let ack = Ack {
      holder: HostId(2),
      object: ObjectId::new(HostId(1), 1),
      sequence: 0,
      generation: 7,
      identity: [0u8; 32],
    }
    .encode();
    assert_eq!(decode_refusal(&ack), None);
    assert_eq!(decode_refusal(&[]), None);
    assert_eq!(decode_refusal(&[STALE_REFUSAL_TAG]), None); // tag but no version
    assert_eq!(decode_refusal(&[9, 0, 0, 0, 0, 0, 0, 0, 0]), None); // right length, unknown tag
  }

  /// AC (§4.8, A-9 — the per-host epoch fence): when the configuration group bumps a failed owner's fencing
  /// epoch, a holder raises its fence to it, and the owner's later records under the **old** epoch are
  /// refused `StaleEpoch` — the immediate fence a configuration install installs, before any per-object
  /// phase-one prepare. Monotonic: raising to a lower epoch is a no-op.
  #[test]
  fn a_raised_fence_refuses_a_record_under_the_old_epoch() {
    let owner = HostId(1);
    let authority = Authority {
      generation: 0,
      owner,
    };
    let mut acceptor = Acceptor::new(HostId(2), authority);
    let object = ObjectId::new(HostId(1), 5);
    // The owner commits a record at epoch 1.
    let at_epoch_1 = Record {
      owner,
      object,
      sequence: 0,
      epoch: HostEpoch(1),
      generation: 0,
      value: b"v1".to_vec(),
    };
    assert!(acceptor.accept(&at_epoch_1).is_ok());

    // The council takes the failed owner over: the committed fencing epoch bumps to 2, and the holder raises
    // its fence to it on installing the configuration.
    acceptor.raise_fence(HostEpoch(2));

    // A resumed stale owner's later record under the old epoch 1 is now refused StaleEpoch.
    let zombie = Record {
      owner,
      object,
      sequence: 1,
      epoch: HostEpoch(1),
      generation: 0,
      value: b"stale".to_vec(),
    };
    assert_eq!(
      acceptor.accept(&zombie),
      Err(RegisterError::StaleEpoch { current: 2 }),
      "a record under the pre-takeover epoch is fenced"
    );

    // Monotonic: raising to a lower epoch does not lower the fence.
    acceptor.raise_fence(HostEpoch(1));
    assert_eq!(
      acceptor.accept(&zombie),
      Err(RegisterError::StaleEpoch { current: 2 }),
      "the fence only strengthens"
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

  /// Copyset placement through `candidates_for` is deterministic and owner-first. At the candidate floor
  /// the neighbourhood is a single copyset every object shares; above the floor objects spread across the
  /// (linearly many) fixed copysets — rendezvous over the copysets, not the raw hosts (research §3.1), so a
  /// per-object candidate set is one copyset, never an arbitrary `2f+1`-subset of a wide neighbourhood.
  #[test]
  fn copyset_candidates_are_deterministic_owner_first_and_spread_above_the_floor() {
    let owner = HostId(1);
    let quorum = Quorum { f: 2 }; // candidate floor 2f+1 = 5
    let domains = std::collections::BTreeMap::new(); // unique-per-host: domains do not constrain here

    // At the floor: a five-host neighbourhood is one copyset, and every object shares it.
    let floor = vec![owner, HostId(2), HostId(3), HostId(4), HostId(5)];
    let one = candidates_for(owner, &floor, &domains, ObjectId::new(owner, 0), quorum);
    assert_eq!(
      one.len(),
      5,
      "the floor neighbourhood is a single copyset of 2f+1"
    );
    assert_eq!(one[0], owner, "owner first");
    for local in 0..256u64 {
      let object = ObjectId::new(owner, local);
      let a = candidates_for(owner, &floor, &domains, object, quorum);
      assert_eq!(
        a,
        candidates_for(owner, &floor, &domains, object, quorum),
        "deterministic"
      );
      assert_eq!(a, one, "at the floor every object shares the one copyset");
    }

    // Above the floor: owner + eight co-holders → 2f=4 per copyset → two fixed copysets the objects spread
    // across, each still a `2f+1` set headed by the owner (the linear-count "without loss" placement).
    let wide: Vec<HostId> = (1..=9u64).map(HostId).collect();
    let mut sets = std::collections::BTreeSet::new();
    for local in 0..256u64 {
      let object = ObjectId::new(owner, local);
      let a = candidates_for(owner, &wide, &domains, object, quorum);
      assert_eq!(a[0], owner, "owner first");
      assert_eq!(
        a.len(),
        5,
        "each candidate set is one copyset of 2f+1, not the whole wide neighbourhood"
      );
      sets.insert(a);
    }
    assert!(
      sets.len() > 1,
      "above the floor objects spread across more than one copyset (a linear, not per-object, count)"
    );
  }

  /// AC (§4.8 "Placement", D-14): with declared failure domains, no copyset holds two hosts of the same
  /// domain, so a domain (rack or machine) failure takes at most one copy of any object. The domain map
  /// overrides the unique-per-host default — hosts that co-hold under unique-per-host are split apart when
  /// they share a declared domain.
  #[test]
  fn a_copyset_never_repeats_a_declared_failure_domain() {
    let owner = HostId(1);
    let neigh = vec![owner, HostId(2), HostId(3), HostId(4), HostId(5)];
    let quorum = Quorum { f: 1 }; // copysets of 2f+1 = 3
    // Two domains of two hosts each (2,3 share; 4,5 share); the owner alone in a third.
    let domains: std::collections::BTreeMap<HostId, DomainId> = [
      (owner, 100u64),
      (HostId(2), 10),
      (HostId(3), 10),
      (HostId(4), 20),
      (HostId(5), 20),
    ]
    .into_iter()
    .collect();
    let mut split = true;
    for local in 0..256u64 {
      let object = ObjectId::new(owner, local);
      let candidates = candidates_for(owner, &neigh, &domains, object, quorum);
      assert_eq!(candidates[0], owner, "owner heads the copyset");
      let doms: Vec<DomainId> = candidates.iter().map(|h| domains[h]).collect();
      let distinct: std::collections::BTreeSet<DomainId> = doms.iter().copied().collect();
      assert_eq!(
        distinct.len(),
        doms.len(),
        "no copyset repeats a failure domain: {candidates:?}"
      );
      if candidates.contains(&HostId(2)) && candidates.contains(&HostId(3)) {
        split = false;
      }
    }
    assert!(split, "hosts 2 and 3 share a domain, so they never co-hold");

    // Non-vacuity: under unique-per-host (an empty map) those same two hosts DO co-hold for some object —
    // the declared domain is what split them, not the object ids or the neighbourhood shape.
    let empty = std::collections::BTreeMap::new();
    let together = (0..256u64).any(|local| {
      let c = candidates_for(owner, &neigh, &empty, ObjectId::new(owner, local), quorum);
      c.contains(&HostId(2)) && c.contains(&HostId(3))
    });
    assert!(
      together,
      "without declared domains 2 and 3 co-hold — so the domain constraint is what forbids it"
    );
  }

  /// AC (§4.8, D-14): a configuration reports its copyset count and coincident-loss probability and checks
  /// the loss against an accepted bound — the durability check the design runs at every configuration
  /// change. Poorer failure domains at the same size make more, shorter copysets and at least as high a
  /// loss (the recovery-vs-durability trade the check surfaces), never a silent over-scatter.
  #[test]
  fn a_configuration_reports_its_copyset_count_and_checks_the_loss_bound() {
    let owner = HostId(1);
    let floor = Configuration {
      version: 0,
      owner,
      host_epoch: FIRST_EPOCH,
      neighbourhood: vec![owner, HostId(2), HostId(3)],
      domains: std::collections::BTreeMap::new(),
      quorum: Quorum { f: 1 },
      has_mirror: false,
      settled: None,
      generation: 0,
    };
    assert_eq!(
      floor.copyset_count(),
      1,
      "the floor neighbourhood is one copyset"
    );
    assert!(
      floor.within_loss_bound(1.0, 2),
      "the floor is within an accept-all bound"
    );

    // Same neighbourhood size, poorer failure domains: the four co-holders share one domain, so no two can
    // co-hold — the greedy makes four short copysets instead of two, a higher coincident loss.
    let rich = Configuration {
      neighbourhood: (1..=5u64).map(HostId).collect(),
      ..floor.clone()
    };
    let one_domain: std::collections::BTreeMap<HostId, DomainId> =
      (2..=5u64).map(|h| (HostId(h), 99)).collect();
    let poor = Configuration {
      domains: one_domain,
      ..rich.clone()
    };
    assert!(
      poor.copyset_count() > rich.copyset_count(),
      "poorer domains make more, shorter copysets at the same size"
    );
    assert!(
      poor.coincident_loss(2) >= rich.coincident_loss(2),
      "more copysets at the same neighbourhood size is at least as high a coincident loss"
    );
    assert!(
      poor.coincident_loss(2) > 0.0,
      "the poor configuration can lose data"
    );
    assert!(
      !poor.within_loss_bound(0.0, 2),
      "a zero accepted-loss bound rejects a configuration that can lose data"
    );
  }

  /// AC-8.1, §4.8: losing the only host loses the object, both on a laptop and while a
  /// newly bootstrapped fleet has not admitted its additional holders. Missing copies are
  /// never counted as protection by the operator's durability check.
  #[test]
  fn a_single_host_cannot_report_zero_loss_by_counting_absent_copies() {
    for quorum in [Quorum { f: 0 }, Quorum { f: 1 }] {
      let configuration = Configuration {
        version: 0,
        owner: HostId(1),
        host_epoch: FIRST_EPOCH,
        neighbourhood: vec![HostId(1)],
        domains: Default::default(),
        quorum,
        has_mirror: false,
        settled: None,
        generation: 0,
      };
      assert!(configuration.within_loss_bound(0.0, 0));
      assert!(!configuration.within_loss_bound(0.0, 1));
      assert_eq!(configuration.coincident_loss(1), 1.0);
    }
  }

  /// AC (§4.8, D-14): the regional configuration the council agrees on derives each owner's single-owner
  /// placement view — its bounded neighbourhood, its host epoch, the failure domains — so every node reads
  /// the same configuration the council fixed, and a non-member has none.
  #[test]
  fn the_regional_configuration_derives_each_owners_placement_view() {
    let members = vec![HostId(1), HostId(2), HostId(3), HostId(4), HostId(5)];
    let scatter = 3; // the candidate floor at f=1
    let regional = RegionalConfiguration::formed(
      members.clone(),
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    for &owner in &members {
      let config = regional
        .configuration_for(owner)
        .expect("a member has a configuration");
      assert_eq!(config.owner, owner);
      assert_eq!(
        config.neighbourhood.len(),
        3,
        "bounded to the scatter width"
      );
      assert_eq!(
        config.neighbourhood[0], owner,
        "the owner heads its own neighbourhood"
      );
      assert_eq!(config.host_epoch, FIRST_EPOCH);
      assert_eq!(config.quorum, Quorum { f: 1 });
    }
    assert!(
      regional.configuration_for(HostId(99)).is_none(),
      "a non-member has no configuration"
    );
  }

  /// §4.8 (2026-10-10): a holder's view of whom it holds for agrees with each owner's own view of its neighbourhood.
  /// Do: for a formed configuration and for one with a change in flight, ask every member pair both ways. Expect
  /// `holds_for(owner, host)` exactly when `owner`'s configuration `neighbours(host)`, settled cohort included.
  #[test]
  fn a_holders_view_agrees_with_each_owners_neighbourhood() {
    let members: Vec<HostId> = (1..=7).map(HostId).collect();
    let formed = RegionalConfiguration::formed(
      members.clone(),
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    let (in_flight, ..) = change_in_flight();
    for regional in [&formed, &in_flight] {
      for &owner in &regional.members {
        let configuration = regional.configuration_for(owner).unwrap();
        for &host in &regional.members {
          assert_eq!(
            regional.holds_for(owner, host),
            configuration.neighbours(host),
            "owner {owner:?}, host {host:?}"
          );
        }
      }
    }
    assert!(
      !formed.holds_for(HostId(99), HostId(1)),
      "a non-member is held by nobody"
    );
  }

  /// AC (§4.8, D-14): admitting and retiring members refixes the neighbourhoods and advances the version, so
  /// a request under a stale version is refused; admitting an existing member is a no-op.
  #[test]
  fn admitting_and_retiring_members_refixes_neighbourhoods_and_advances_the_version() {
    let scatter = 3;
    let mut regional = RegionalConfiguration::formed(
      vec![HostId(1), HostId(2), HostId(3)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    let before = regional.version;
    assert!(
      regional.admit(HostId(4), None, scatter),
      "a new member is admitted"
    );
    assert!(regional.version > before, "the version advanced");
    assert!(regional.members.contains(&HostId(4)));
    assert!(
      regional.configuration_for(HostId(4)).is_some(),
      "the admitted member now has a placement view"
    );
    assert!(
      !regional.admit(HostId(4), None, scatter),
      "admitting an existing member is a no-op"
    );
    assert!(regional.retire(HostId(2), scatter), "a member is retired");
    assert!(!regional.members.contains(&HostId(2)));
    assert!(
      regional.configuration_for(HostId(2)).is_none(),
      "a retired member has no configuration"
    );
  }

  /// AC (§4.8, D-14; task #22): an admission carries the failure domain the member's node declares, so the
  /// member places under it at once (a restarted node's new id inherits its node's domain this way), and a
  /// retirement drops the domain again, so the map stays bounded to the members while every restart admits
  /// a new id (banned item 8).
  #[test]
  fn an_admission_carries_the_members_domain_and_a_retirement_drops_it() {
    let scatter = 3;
    let mut regional = RegionalConfiguration::formed(
      vec![HostId(1), HostId(2), HostId(3)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    // Shape: the failure domain the admitted member's node declares — any id distinct from unique-per-host.
    let declared_domain: DomainId = 7;
    assert!(regional.admit(HostId(4), Some(declared_domain), scatter));
    assert_eq!(
      regional
        .configuration_for(HostId(4))
        .and_then(|configuration| configuration.domains.get(&HostId(4)).copied()),
      Some(declared_domain),
      "the admitted member places under the failure domain its admission carried"
    );
    assert!(regional.retire(HostId(4), scatter));
    assert!(
      !regional.domains.contains_key(&HostId(4)),
      "a retired member's domain is dropped — the map stays bounded to the members"
    );
  }

  /// AC (§4.8, D-14): a takeover bumps the dead host's fencing epoch (so its stale records are refused) and
  /// retires it, keeping the bumped epoch so a returning stale host is still fenced.
  #[test]
  fn a_takeover_bumps_the_dead_hosts_epoch_and_retires_it() {
    let scatter = 3;
    let mut regional = RegionalConfiguration::formed(
      vec![HostId(1), HostId(2), HostId(3)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    let before = regional.epochs[&HostId(2)];
    let bumped = regional.take_over(HostId(2), scatter).unwrap();
    assert!(
      bumped.0 > before.0,
      "the dead host's epoch is bumped, fencing its in-flight records"
    );
    assert!(
      !regional.members.contains(&HostId(2)),
      "the dead host is retired from the region"
    );
    assert_eq!(
      regional.epochs.get(&HostId(2)),
      Some(&bumped),
      "the bumped epoch is kept, so a returning stale host is still fenced"
    );
  }

  /// Shape: a region wider than its scatter width, so a membership change moves some neighbourhoods and not
  /// others (five members, three hosts to a neighbourhood at `f = 1`).
  fn wide_region() -> (RegionalConfiguration, u64) {
    let scatter = 3;
    let regional = RegionalConfiguration::formed(
      (1..=5).map(HostId).collect(),
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    (regional, scatter)
  }

  /// §4.8 "Neighbourhood changes": a neighbourhood keeps the version its host set was fixed at while a change
  /// leaves that set as it was, and takes the new version when the set moves — so an owner can tell whether its
  /// neighbourhood moved since the one its records are all placed on.
  #[test]
  fn a_neighbourhood_keeps_its_version_until_its_host_set_moves() {
    let (mut regional, scatter) = wide_region();
    let owner = HostId(1);
    let before = regional.neighbourhoods[&owner].clone();
    let outside = regional
      .members
      .iter()
      .copied()
      .find(|host| !before.hosts.contains(host))
      .expect("a five-member region has a host outside a three-host neighbourhood");
    assert!(regional.retire(outside, scatter));
    assert_eq!(
      regional.neighbourhoods[&owner], before,
      "retiring a host outside the neighbourhood leaves its set and version as they were"
    );
    let inside = before
      .hosts
      .iter()
      .copied()
      .find(|host| *host != owner)
      .expect("a neighbourhood holds hosts besides its owner");
    assert!(regional.retire(inside, scatter));
    let after = &regional.neighbourhoods[&owner];
    assert_ne!(
      after.hosts, before.hosts,
      "retiring a neighbour moves the set"
    );
    assert_eq!(
      after.generation, regional.version,
      "a moved set takes the version that moved it"
    );
  }

  /// §4.8 "Neighbourhood changes" (Vertical Paxos II): a formed region and an admitted member own nothing, so
  /// their first neighbourhoods are settled at once.
  #[test]
  fn a_formed_or_admitted_member_starts_settled() {
    let (mut regional, scatter) = wide_region();
    for (owner, neighbourhood) in &regional.neighbourhoods {
      assert_eq!(
        regional.settled[owner].hosts, neighbourhood.hosts,
        "a formed member's first neighbourhood is settled"
      );
    }
    let joiner = HostId(6);
    assert!(regional.admit(joiner, None, scatter));
    assert_eq!(
      regional.settled[&joiner].hosts, regional.neighbourhoods[&joiner].hosts,
      "an admitted member owns nothing, so its first neighbourhood is settled"
    );
  }

  /// §4.8 "Neighbourhood changes" (Vertical Paxos II): a member whose neighbourhood an admission moved keeps its
  /// settled set until it reports the new one placed, and only a report of its current neighbourhood's version
  /// moves it, once.
  #[test]
  fn a_settled_neighbourhood_moves_only_on_its_owners_report_of_the_current_set() {
    let (mut regional, scatter) = wide_region();
    let joiner = HostId(6);
    assert!(regional.admit(joiner, None, scatter));
    let moved = regional
      .members
      .iter()
      .copied()
      .find(|owner| regional.neighbourhoods[owner].hosts.contains(&joiner) && *owner != joiner)
      .expect("the joiner entered some neighbourhood");
    let current = regional.neighbourhoods[&moved].generation;
    assert_ne!(
      regional.settled[&moved].generation, current,
      "the neighbourhood the joiner entered is not settled until its owner reports it"
    );
    let version = regional.version;
    assert!(
      !regional.settle(moved, current - 1),
      "a report of a set that has moved on since changes nothing"
    );
    assert!(
      regional.settle(moved, current),
      "the owner's report settles it"
    );
    assert_eq!(
      regional.settled[&moved].hosts,
      regional.neighbourhoods[&moved].hosts
    );
    assert_eq!(
      regional.version,
      version + 1,
      "every node learns the old set is retired"
    );
    assert!(
      !regional.settle(moved, current),
      "a repeated report changes nothing"
    );
  }

  /// §4.8 "Promotion and takeover": a retirement records the owner's **settled** neighbourhood, not the one a
  /// change in flight moved it to, since the owner's records are all placed only on the settled one; every node
  /// then names the same successor for each object from the configuration alone, never a retired host.
  #[test]
  fn a_retirement_recovers_through_the_settled_neighbourhood_and_names_one_successor() {
    let (mut regional, scatter) = wide_region();
    let owner = HostId(1);
    let joiner = HostId(6);
    assert!(regional.admit(joiner, None, scatter));
    let settled = regional.settled[&owner].clone();
    regional.take_over(owner, scatter);
    let retirement = &regional.retired[&owner];
    assert_eq!(
      retirement.settled, settled,
      "the settled set is what the owner's records are on"
    );
    assert_eq!(retirement.version, regional.version);
    assert!(retirement.confirmed.is_empty() && retirement.unconfirmed.is_empty());
    let object = ObjectId::new(owner, 7);
    let cohorts = regional.recovery_cohorts(owner, object);
    assert_eq!(
      cohorts,
      vec![settled.cohort(owner, object, regional.quorum)],
      "one cohort: the one the owner placed the object on"
    );
    let successor = regional
      .successor(owner, object)
      .expect("the cohort has survivors");
    assert!(
      cohorts[0].contains(&successor) && regional.members.contains(&successor),
      "the successor is a surviving member of the cohort"
    );
    let decoded = regional.clone();
    assert_eq!(
      decoded.successor(owner, object),
      Some(successor),
      "the successor is a function of the configuration alone"
    );
  }

  /// §4.8 "only then serves under confirmed authority": a retirement is kept until every surviving host of its
  /// recovery neighbourhoods has confirmed its share, and dropped with the last confirmation; a confirmation
  /// from a host that owes none changes nothing.
  #[test]
  fn a_retirement_is_dropped_once_every_survivor_confirms_its_share() {
    let (mut regional, scatter) = wide_region();
    let departed = HostId(1);
    regional.take_over(departed, scatter);
    let owing: Vec<HostId> = regional
      .recovery_hosts(departed)
      .into_iter()
      .filter(|host| regional.owes_confirmation(departed, *host))
      .collect();
    assert!(
      !owing.is_empty(),
      "the departed host's neighbours owe confirmations"
    );
    assert!(
      !regional.confirm(departed, departed),
      "the departed host owes nothing"
    );
    let (last, first) = owing.split_last().expect("some host owes");
    for host in first {
      assert!(regional.confirm(departed, *host));
      assert!(
        regional.retired.contains_key(&departed),
        "kept while a survivor owes"
      );
    }
    assert!(regional.confirm(departed, *last));
    assert!(
      !regional.retired.contains_key(&departed),
      "dropped with the last confirmation"
    );
  }

  /// §4.8 "Authority scope" and "Lookup", §4.10 "operators keep an explicit move" (T-8.14's configuration half). Do: in
  /// a five-member region, move host 1's object to host 2, then try every move that is not its owner's; move it on to
  /// host 3; move it back to its creator. Expect each owner's move to change the configuration once, the exception to
  /// name the owner at the move's version and the member the move took it from, every other move refused (not the
  /// owner, to itself, to a non-member), and the move back to the creator to keep an exception naming the creator and
  /// host 3 (A-291: the creator released its copy and adopts it back as any member does); the object's catalog register
  /// moves with it, and an unrelated object of host 1 never does.
  #[test]
  fn a_move_names_the_objects_owner_and_only_its_owner_moves_it() {
    let (mut regional, _) = wide_region();
    let (creator, b, c) = (HostId(1), HostId(2), HostId(3));
    let object = ObjectId::new(creator, 7);
    let unrelated = ObjectId::new(creator, 8);
    // The cohorts the records sit in, each mover among its own (given out of order: the exception keeps id order).
    let (creators_cohort, bs_cohort, cs_cohort) = (
      [HostId(4), creator, HostId(3)],
      [b, HostId(5), HostId(4)],
      [c, HostId(1), HostId(2)],
    );
    assert_eq!(regional.owner_of(object), Some(creator));

    assert!(
      !regional.move_object(object, creator, b, &[b, HostId(4)]),
      "a cohort without the mover is no placement of its records"
    );
    assert!(!regional.move_object(object, creator, b, &[]));
    let before = regional.version;
    let version = expect_a_move_to_name_its_new_owner(
      &mut regional,
      object,
      unrelated,
      (creator, b),
      &creators_cohort,
    );
    expect_every_move_but_its_owners_refused(
      &mut regional,
      object,
      (creator, b, c),
      &creators_cohort,
      &bs_cohort,
    );
    assert_eq!(regional.version, version, "refused moves change nothing");
    expect_a_move_back_to_its_creator_kept_as_an_exception(
      &mut regional,
      object,
      (creator, b, c),
      (&bs_cohort, &cs_cohort),
      before,
    );
  }

  /// The end of `a_move_names_the_objects_owner_and_only_its_owner_moves_it`. Do: with `object` moved from `creator`
  /// to `b` at version `before + 1`, move it on to `c`, then back to `creator`, then from `c` to `b`. Expect the move
  /// back to keep an exception naming the creator and `c` at version `before + 3` (A-291: the creator released its copy
  /// and adopts it back as any member does), and the member it left to move it no more.
  fn expect_a_move_back_to_its_creator_kept_as_an_exception(
    regional: &mut RegionalConfiguration,
    object: ObjectId,
    (creator, b, c): (HostId, HostId, HostId),
    (bs_cohort, cs_cohort): (&[HostId], &[HostId]),
    before: u64,
  ) {
    assert!(regional.move_object(object, b, c, bs_cohort));
    assert_eq!(regional.owner_of(object), Some(c));
    assert!(regional.move_object(object, c, creator, cs_cohort));
    assert_eq!(
      regional.moved.get(&object),
      Some(&Moved {
        owner: creator,
        version: before + 3,
        from: Some(c),
        cohort: vec![creator, b, c],
      }),
      "back with its creator under an exception the creator's adoption follows"
    );
    assert_eq!(regional.owner_of(object), Some(creator));
    assert!(
      !regional.move_object(object, c, b, cs_cohort),
      "the member it left moves it no more"
    );
  }

  /// A-291 (§4.10 "Ownership follows the writer": "the new owner runs phase one"). Do: move host 1's object to host 2;
  /// report it adopted from host 3, then from host 2, then again from host 2; then move it on to host 3 and report that
  /// move adopted. Expect only the owner's first report to change the configuration: the exception keeps its owner and
  /// ownership generation and drops the member the move left and the move's cohort, which is that member's word to let
  /// its copy go, and a second report changes nothing; each move owes its own adoption.
  #[test]
  fn a_moves_adoption_is_reported_by_its_new_owner_and_lets_the_member_it_left_go() {
    let (mut regional, _) = wide_region();
    let (creator, b, c) = (HostId(1), HostId(2), HostId(3));
    let object = ObjectId::new(creator, 7);
    assert!(regional.move_object(object, creator, b, &[creator, c, HostId(4)]));
    let moved_at = regional.version;
    assert!(!regional.adopt_move(object, c), "only its owner adopts it");
    assert_eq!(regional.version, moved_at);
    assert!(regional.adopts_move(object, b));
    assert!(regional.adopt_move(object, b));
    assert_eq!(
      regional.moved.get(&object),
      Some(&Moved {
        owner: b,
        version: moved_at,
        from: None,
        cohort: Vec::new(),
      }),
      "adopted: the owner and its generation kept, the member it left let go"
    );
    assert_eq!(regional.version, moved_at + 1);
    expect_each_move_to_owe_its_own_adoption(&mut regional, object, b, c);
  }

  /// The end of `a_moves_adoption_is_reported_by_its_new_owner_and_lets_the_member_it_left_go`. Do: with `object`'s
  /// move to `owner` adopted, report it adopted again; move it on to `next` and report that move adopted. Expect the
  /// second report refused and the next move to owe, and take, its own adoption.
  fn expect_each_move_to_owe_its_own_adoption(
    regional: &mut RegionalConfiguration,
    object: ObjectId,
    owner: HostId,
    next: HostId,
  ) {
    assert!(
      !regional.adopt_move(object, owner),
      "an adopted move is adopted once"
    );
    assert_eq!(regional.owner_of(object), Some(owner));
    assert!(regional.move_object(object, owner, next, &[owner, HostId(4), HostId(5)]));
    assert!(
      regional.adopts_move(object, next),
      "the next move owes its own"
    );
    assert!(regional.adopt_move(object, next));
    assert_eq!(regional.owner_of(object), Some(next));
  }

  /// The first move of `a_move_names_the_objects_owner_and_only_its_owner_moves_it`. Do: move `object` from
  /// `creator`, which made `unrelated` too, to `to`, its records in `cohort`. Expect the configuration's version to
  /// advance by one, the exception to name `to` at that version with the member it left and the cohort (A-291), the
  /// object and its catalog register owned by `to`, and `unrelated` still on its creator's line. Returns the version
  /// the move committed at.
  fn expect_a_move_to_name_its_new_owner(
    regional: &mut RegionalConfiguration,
    object: ObjectId,
    unrelated: ObjectId,
    (creator, to): (HostId, HostId),
    cohort: &[HostId],
  ) -> u64 {
    let before = regional.version;
    assert!(regional.move_object(object, creator, to, cohort));
    assert_eq!(regional.version, before + 1);
    // The exception keeps the cohort in id order (A-291).
    let mut in_order = cohort.to_vec();
    in_order.sort_unstable_by_key(|host| host.0);
    in_order.dedup();
    assert_eq!(
      regional.moved.get(&object),
      Some(&Moved {
        owner: to,
        version: before + 1,
        from: Some(creator),
        cohort: in_order,
      })
    );
    assert_eq!(regional.owner_of(object), Some(to));
    assert_eq!(
      regional.owner_of(object.catalog()),
      Some(to),
      "a volume's catalog register moves with it"
    );
    assert_eq!(
      regional.owner_of(unrelated),
      Some(creator),
      "a move fences nothing but its object"
    );
    regional.version
  }

  /// The refused moves of `a_move_names_the_objects_owner_and_only_its_owner_moves_it`. Do: with `object` moved from
  /// `creator` to `owner`, move it from its old owner to `other`, from its owner to itself, and from its owner to a
  /// host that is not a member. Expect every one refused.
  fn expect_every_move_but_its_owners_refused(
    regional: &mut RegionalConfiguration,
    object: ObjectId,
    (creator, owner, other): (HostId, HostId, HostId),
    creators_cohort: &[HostId],
    owners_cohort: &[HostId],
  ) {
    assert!(
      !regional.move_object(object, creator, other, creators_cohort),
      "the old owner no longer moves it"
    );
    assert!(
      !regional.move_object(object, owner, owner, owners_cohort),
      "not to its own owner"
    );
    assert!(
      !regional.move_object(object, owner, HostId(9), owners_cohort),
      "not to a non-member"
    );
  }

  /// §4.8 "Authority scope" with "Promotion and takeover". Do: move host 1's object to host 2; take host 2 over;
  /// confirm every survivor's share. Expect the object owned by its successor while the retirement is kept, the
  /// exception naming that successor from its confirmation on, and still the successor once the retirement is
  /// dropped, never the creator. Then report it destroyed: only its owner's report drops the exception, after which
  /// it is on its creator's line again.
  #[test]
  fn a_moved_objects_exception_follows_its_owners_takeover_and_goes_with_its_destroy() {
    let (mut regional, scatter) = wide_region();
    let (creator, b) = (HostId(1), HostId(2));
    let object = ObjectId::new(creator, 7);
    assert!(regional.move_object(object, creator, b, &[creator]));
    regional.take_over(b, scatter);
    let Lineage::Successor { successor, .. } = regional.lineage(b, object) else {
      panic!("a takeover with survivors names a successor");
    };
    assert_eq!(regional.owner_of(object), Some(successor));
    for host in regional.recovery_hosts(b) {
      regional.confirm(b, host);
    }
    assert!(
      !regional.retired.contains_key(&b),
      "the retirement is dropped"
    );
    assert_eq!(
      regional
        .moved
        .get(&object)
        .map(|moved| (moved.owner, moved.from)),
      Some((successor, None)),
      "the exception followed the takeover, whose own round adopted the object (no move's round owed)"
    );
    assert_eq!(regional.owner_of(object), Some(successor));
    expect_only_its_owners_destroy_report_drops_the_exception(
      &mut regional,
      object,
      creator,
      successor,
    );
  }

  /// The destroy of `a_moved_objects_exception_follows_its_owners_takeover_and_goes_with_its_destroy`. Do: with
  /// `object`, made by `creator`, moved and owned by `owner`, report it destroyed from another member, then from its
  /// owner, then from its owner again. Expect only its owner's first report to drop the exception, after which the
  /// object is on its creator's line again and there is nothing left to forget.
  fn expect_only_its_owners_destroy_report_drops_the_exception(
    regional: &mut RegionalConfiguration,
    object: ObjectId,
    creator: HostId,
    owner: HostId,
  ) {
    let other = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != owner)
      .expect("a five-member region keeps another member");
    assert!(
      !regional.forget_moved(object, other),
      "only its owner reports it destroyed"
    );
    assert!(regional.forget_moved(object, owner));
    assert!(!regional.moved.contains_key(&object));
    assert_eq!(regional.owner_of(object), Some(creator));
    assert!(
      !regional.forget_moved(object, owner),
      "nothing left to forget"
    );
  }

  /// §4.8 "Authority scope" with "Promotion and takeover", repeated. Do: move host 1's objects to host 2; take host 2
  /// over; take over, before it confirms, the survivor that is the successor of one of them, so its own takeover carries
  /// the first; confirm every share of both. Expect that object owned throughout by the member its line names, its
  /// exception naming the member that adopted it once both retirements are dropped, never a lost line (before
  /// 2026-11-18 the exception followed only a successor that confirmed the very takeover the move's owner retired in,
  /// so it kept naming host 2 and the object's line read settled, with no owner). And an object whose successor is its
  /// creator is back on its creator's line, with no exception.
  #[test]
  fn a_moved_objects_exception_follows_a_takeover_carried_into_another() {
    let (mut regional, scatter) = wide_region();
    let (creator, moved_to) = (HostId(1), HostId(2));
    let objects = objects_moved(&mut regional, creator, moved_to);
    regional.take_over(moved_to, scatter);
    let successor_of = |regional: &RegionalConfiguration, object: ObjectId| match regional
      .lineage(moved_to, object)
    {
      Lineage::Successor { successor, .. } => successor,
      other => panic!("a takeover with survivors names a successor: {other:?}"),
    };
    let carried = objects
      .iter()
      .copied()
      .find(|object| successor_of(&regional, *object) != creator)
      .expect("some object's successor is not its creator");
    let carrier = successor_of(&regional, carried);
    let home = objects
      .iter()
      .copied()
      .find(|object| successor_of(&regional, *object) == creator);
    regional.take_over(carrier, scatter);
    let adopter = regional
      .owner_of(carried)
      .expect("the carried takeover names a member");
    assert_ne!(adopter, carrier);
    for host in regional.recovery_hosts(moved_to) {
      regional.confirm(moved_to, host);
    }
    for host in regional.recovery_hosts(carrier) {
      regional.confirm(carrier, host);
    }
    assert!(regional.retired.is_empty(), "both takeovers are done");
    assert_eq!(
      regional.owner_of(carried),
      Some(adopter),
      "the member that adopted it owns it, once the line it was adopted through is dropped"
    );
    assert_eq!(
      regional.moved.get(&carried).map(|moved| moved.owner),
      (adopter != creator).then_some(adopter)
    );
    expect_every_moved_line_kept(&regional, &objects, home, creator);
  }

  /// The objects of `a_moved_objects_exception_follows_a_takeover_carried_into_another`: sixty-four of `creator`'s,
  /// each moved to `to`. Expect every move made.
  fn objects_moved(
    regional: &mut RegionalConfiguration,
    creator: HostId,
    to: HostId,
  ) -> Vec<ObjectId> {
    let objects: Vec<ObjectId> = (0..64).map(|index| ObjectId::new(creator, index)).collect();
    for object in &objects {
      assert!(regional.move_object(*object, creator, to, &[creator]));
    }
    objects
  }

  /// The end of `a_moved_objects_exception_follows_a_takeover_carried_into_another`, both takeovers done. Expect
  /// `home`, an object its creator `creator` adopted if there was one, back on its creator's line with no exception,
  /// and every one of `objects` still owned by a member.
  fn expect_every_moved_line_kept(
    regional: &RegionalConfiguration,
    objects: &[ObjectId],
    home: Option<ObjectId>,
    creator: HostId,
  ) {
    if let Some(home) = home {
      assert!(
        !regional.moved.contains_key(&home),
        "an object adopted by its creator is back on its creator's line"
      );
      assert_eq!(regional.owner_of(home), Some(creator));
    }
    for object in objects {
      assert!(
        regional.owner_of(*object).is_some(),
        "no moved object's line is lost: {object:?}"
      );
    }
  }

  /// §4.10 "operators keep an explicit move" after "Promotion and takeover". Do: take host 1 over and confirm every
  /// share, so its takeover completes and the configuration names no owner for its objects; move one from the member
  /// that adopted it; then let the completion record go, as its bound does, so another of its objects' line reads
  /// unresolved, and move that one from a member. Expect the settled object's move made by a member, its exception
  /// naming the member it moved to; a move from a non-member or to the mover refused; and the unresolved object's move
  /// made too (A-309): the configuration names no owner there either and holds no claim against the member that serves
  /// it, and refusing would leave every object whose creator's record was let go unmovable for good.
  #[test]
  fn a_member_moves_an_object_whose_line_names_no_owner() {
    let (mut regional, scatter) = wide_region();
    let departed = HostId(1);
    let (object, other) = (ObjectId::new(departed, 4), ObjectId::new(departed, 5));
    regional.take_over(departed, scatter);
    let successor = successor_of(&regional, departed, object);
    let adopter = successor_of(&regional, departed, other);
    for host in regional.recovery_hosts(departed) {
      regional.confirm(departed, host);
    }
    assert!(regional.retired.is_empty());
    expect_a_settled_line_moved_by_its_adopter(&mut regional, departed, object, successor);
    // The record let go, as `bound_completed` does past the bound (`completion_records_stay_bounded_…` drives it).
    regional.completed.remove(&departed);
    expect_an_unresolved_line_moved_by_its_adopter(&mut regional, departed, other, adopter);
  }

  /// The settled half of `a_member_moves_an_object_whose_line_names_no_owner`: `object`'s line, settled by
  /// `departed`'s completion record, names no owner, and only `successor` (the member that adopted it) moves it, to
  /// another member.
  fn expect_a_settled_line_moved_by_its_adopter(
    regional: &mut RegionalConfiguration,
    departed: HostId,
    object: ObjectId,
    successor: HostId,
  ) {
    assert_eq!(
      regional.lineage(departed, object),
      Lineage::Settled { takeover: departed }
    );
    assert_eq!(
      regional.owner_of(object),
      None,
      "a done takeover's line names no owner"
    );
    let to = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != successor)
      .expect("a five-member region keeps another member");
    assert!(
      !regional.moves(object, departed, to),
      "a retired host moves nothing"
    );
    assert!(
      !regional.moves(object, successor, successor),
      "not to itself"
    );
    assert!(regional.move_object(object, successor, to, &[successor]));
    assert_eq!(regional.owner_of(object), Some(to));
    assert!(
      !regional.moves(object, successor, departed),
      "once moved, only its new owner moves it"
    );
  }

  /// The unresolved half of `a_member_moves_an_object_whose_line_names_no_owner` (A-309): `object`'s line reads
  /// unresolved once `departed`'s record was let go, names no owner, and `adopter` (the member that adopted it) moves
  /// it.
  fn expect_an_unresolved_line_moved_by_its_adopter(
    regional: &mut RegionalConfiguration,
    departed: HostId,
    object: ObjectId,
    adopter: HostId,
  ) {
    assert_eq!(regional.lineage(departed, object), Lineage::Unresolved);
    assert_eq!(regional.owner_of(object), None);
    let target = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != adopter)
      .expect("a four-member region keeps another member");
    assert!(
      regional.move_object(object, adopter, target, &[adopter]),
      "the member that adopted it moves an object whose record was let go"
    );
    assert_eq!(regional.owner_of(object), Some(target));
  }

  /// A-272, A-309: a lost line is claimed by no move. Do: take over the one member of a region formed on it alone, so
  /// its retirement has no survivor at all; then admit two members and try every move of its object between them.
  /// Expect its line lost and every move refused: no member can have adopted an object no survivor was left to recover.
  #[test]
  fn no_member_moves_a_lost_object() {
    let scatter = 3;
    let mut regional = RegionalConfiguration::formed(
      vec![HostId(1)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    regional.take_over(HostId(1), scatter);
    assert!(regional.admit(HostId(3), None, scatter));
    assert!(regional.admit(HostId(4), None, scatter));
    let object = ObjectId::new(HostId(1), 4);
    assert_eq!(regional.lineage(HostId(1), object), Lineage::Lost);
    for (from, to) in [(HostId(3), HostId(4)), (HostId(4), HostId(3))] {
      assert!(
        !regional.moves(object, from, to),
        "no member claims a lost line"
      );
    }
  }

  /// §4.8 "Promotion and takeover", repeated: a survivor that retires before confirming its share of an earlier
  /// takeover carries that takeover into its own — its objects are recovered jointly through both settled
  /// cohorts, since one it was taking over may not have been re-committed under it — and the earlier retirement
  /// is kept while the later one recovers through it.
  #[test]
  fn a_survivor_retiring_unconfirmed_carries_the_earlier_takeover_into_its_own() {
    let (mut regional, scatter) = wide_region();
    let first = HostId(1);
    regional.take_over(first, scatter);
    let second = regional
      .recovery_hosts(first)
      .into_iter()
      .find(|host| regional.owes_confirmation(first, *host))
      .expect("a survivor owes a confirmation");
    regional.take_over(second, scatter);
    assert_eq!(regional.retired[&second].unconfirmed, vec![first]);
    let mut owners = vec![first, second];
    owners.sort_unstable_by_key(|host| host.0);
    assert_eq!(regional.recovery_owners(second), owners);
    let object = ObjectId::new(second, 3);
    assert_eq!(
      regional.recovery_cohorts(second, object).len(),
      2,
      "recovered through both cohorts"
    );
    for host in regional.recovery_hosts(first) {
      regional.confirm(first, host);
    }
    assert!(
      regional.retired.contains_key(&first),
      "kept while the later retirement recovers through it"
    );
    for host in regional.recovery_hosts(second) {
      regional.confirm(second, host);
    }
    assert!(
      regional.retired.is_empty(),
      "both dropped once both are confirmed"
    );
  }

  /// A wide region in which an admission moved some member's neighbourhood: the region, that member, its
  /// placement configuration while the change is in flight, and the objects whose cohorts the change moved and
  /// did not move.
  fn change_in_flight() -> (
    RegionalConfiguration,
    HostId,
    Configuration,
    Vec<ObjectId>,
    Vec<ObjectId>,
  ) {
    let (mut regional, scatter) = wide_region();
    let joiner = HostId(6);
    assert!(regional.admit(joiner, None, scatter));
    let owner = regional
      .members
      .iter()
      .copied()
      .find(|owner| {
        *owner != joiner
          && regional.settled[owner].generation != regional.neighbourhoods[owner].generation
      })
      .expect("the joiner entered some neighbourhood");
    let configuration = regional.configuration_for(owner).unwrap();
    let settled = configuration
      .settled
      .clone()
      .expect("a change in flight carries the settled neighbourhood");
    let (moved, kept) = (0..64u64)
      .map(|index| ObjectId::new(owner, index))
      .partition(|object| {
        settled.cohort(owner, *object, configuration.quorum)
          != configuration.place_content(*object).candidates
      });
    (regional, owner, configuration, moved, kept)
  }

  /// §4.8 "Neighbourhood changes" ("the group retires the old set only after … the newest committed record is
  /// held by f+1 of the new candidates"): readiness to settle is judged on the current cohort alone. An owner
  /// admitted beside the bootstrap alone is settled on those two hosts; when the bootstrap dies before the
  /// owner reports its later neighbourhood, its old cohort can never again gather `f + 1`, so a joint write
  /// cannot place — and readiness judged on the joint shape would keep the owner unsettled, and every write it
  /// makes unplaced, for good. Its head held by `f + 1` of the current cohort is what lets it settle.
  #[test]
  fn an_owner_whose_old_cohort_lost_a_host_is_ready_once_its_current_cohort_holds_its_head() {
    let quorum = Quorum { f: 1 };
    let (bootstrap, owner, third) = (HostId(1), HostId(2), HostId(3));
    let mut regional = RegionalConfiguration::formed(
      vec![bootstrap],
      quorum,
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    assert!(regional.admit(owner, None, 3));
    assert!(regional.admit(third, None, 3));
    regional.take_over(bootstrap, 3);
    let configuration = regional.configuration_for(owner).unwrap();
    let object = ObjectId::new(owner, 0);
    let old = configuration
      .settled
      .as_ref()
      .map(|settled| settled.cohort(owner, object, quorum))
      .unwrap_or_default();
    assert_eq!(
      old,
      vec![owner, bootstrap],
      "the owner is still settled beside the bootstrap"
    );
    assert!(
      !configuration
        .place(object)
        .placed_with(&[owner, third], quorum),
      "a joint write cannot place while the old cohort has lost a host"
    );
    assert!(
      configuration.placed_on_current(object, &[owner, third]),
      "the head at f + 1 of the current cohort readies the settlement"
    );
    assert!(
      !configuration.placed_on_current(object, &[owner, bootstrap]),
      "the old cohort's acknowledgements do not count toward the new one"
    );
  }

  /// §4.8 "Neighbourhood changes" (joint writes): while an owner's change is in flight, a record of an object
  /// whose cohort the change moved goes to both cohorts and commits only at `f + 1` of **each**, and the cohort
  /// a takeover recovers through — the one the lease counts over — is the settled one.
  #[test]
  fn a_change_in_flight_commits_a_moved_objects_records_on_both_cohorts() {
    let (_, owner, configuration, moved, _) = change_in_flight();
    let quorum = configuration.quorum;
    let object = *moved
      .first()
      .expect("the change moved some object's cohort");
    let old = configuration
      .settled
      .as_ref()
      .unwrap()
      .cohort(owner, object, quorum);
    let new = configuration.place_content(object).candidates;
    let placement = configuration.place(object);
    assert_eq!(placement.joint, vec![old.clone(), new.clone()]);
    assert!(
      old
        .iter()
        .chain(&new)
        .all(|host| placement.candidates.contains(host)),
      "the record goes to both cohorts"
    );
    let gained = *new.iter().find(|host| !old.contains(host)).unwrap();
    let lost = *old.iter().find(|host| !new.contains(host)).unwrap();
    assert!(
      !placement.placed_with(&[owner, gained], quorum),
      "f + 1 of the new cohort alone does not commit"
    );
    assert!(
      !placement.placed_with(&[owner, lost], quorum),
      "f + 1 of the settled cohort alone does not commit"
    );
    assert!(
      placement.placed_with(&[owner, gained, lost], quorum),
      "f + 1 of each commits"
    );
    assert_eq!(
      configuration.lease_cohorts(object),
      vec![old, new],
      "the lease holds in both cohorts: a takeover recovers through the settled one, or through the current \
       one once the council settles it"
    );
    let standing = configuration.standing();
    assert!(
      standing.settled < standing.current,
      "a change in flight: the settled neighbourhood is the older"
    );
  }

  /// §4.8 "Neighbourhood changes": an object whose cohort a change left as it was keeps its one cohort, and
  /// once the council settles the new neighbourhood every record is placed on one cohort again — the new one,
  /// which a takeover then recovers through.
  #[test]
  fn a_settled_change_places_every_record_on_one_cohort_again() {
    let (mut regional, owner, configuration, moved, kept) = change_in_flight();
    if let Some(unmoved) = kept.first() {
      assert!(
        configuration.place(*unmoved).joint.is_empty(),
        "an object whose cohort did not move keeps its one cohort"
      );
    }
    let object = *moved.first().unwrap();
    let new = configuration.place_content(object).candidates;
    assert!(regional.settle(owner, regional.neighbourhoods[&owner].generation));
    let settled = regional.configuration_for(owner).unwrap();
    assert!(
      settled.settled.is_none(),
      "a settled neighbourhood carries no second cohort"
    );
    assert!(settled.place(object).joint.is_empty());
    assert_eq!(settled.lease_cohorts(object), vec![new]);
    assert_eq!(
      settled.standing().settled,
      configuration.standing().current,
      "the settled neighbourhood is the one the owner reported"
    );
    assert!(
      configuration
        .standing()
        .recognizes(settled.standing().settled),
      "a holder that installed the settlement is recognized by the owner that reported it"
    );
  }

  /// A retired host's successor for `object` in `regional`, which must exist.
  fn successor_of(regional: &RegionalConfiguration, departed: HostId, object: ObjectId) -> HostId {
    regional
      .successor(departed, object)
      .expect("a retirement with survivors ranks a successor")
  }

  /// §4.8 "Promotion and takeover": a retirement ranks its objects' successors among the members it left, fixed
  /// at the retirement — a member that joins later is never ranked, and a survivor that retires later is still
  /// the one ranked, so the object's line runs through its own retirement.
  #[test]
  fn a_retirement_ranks_successors_among_the_members_it_left() {
    let (mut regional, scatter) = wide_region();
    let departed = HostId(1);
    let survivors_before: Vec<HostId> = regional.neighbourhoods[&departed]
      .hosts
      .iter()
      .copied()
      .filter(|host| *host != departed)
      .collect();
    regional.take_over(departed, scatter);
    assert_eq!(regional.retired[&departed].survivors, survivors_before);
    let object = ObjectId::new(departed, 11);
    let successor = successor_of(&regional, departed, object);
    assert!(regional.admit(HostId(7), None, scatter));
    assert_eq!(
      successor_of(&regional, departed, object),
      successor,
      "a later joiner is never ranked"
    );
    regional.take_over(successor, scatter);
    assert_eq!(
      successor_of(&regional, departed, object),
      successor,
      "a survivor that retired later is still the one ranked"
    );
  }

  /// §4.8 "Promotion and takeover", repeated: an object whose successor retired before confirming is recovered
  /// in the successor's takeover, and a holder that last saw the object under the first owner resolves the same
  /// member as one that saw it under the successor — the lineage is the configuration's, not the order a node
  /// installed it in.
  #[test]
  fn a_lineage_follows_a_successor_that_retired_before_confirming() {
    let (mut regional, scatter) = wide_region();
    let first = HostId(1);
    regional.take_over(first, scatter);
    let object = (0..64u64)
      .map(|index| ObjectId::new(first, index))
      .find(|object| {
        regional
          .survivors_of(first)
          .contains(&successor_of(&regional, first, *object))
      })
      .expect("an object of the first owner");
    let carrier = successor_of(&regional, first, object);
    regional.take_over(carrier, scatter);
    assert_eq!(regional.retired[&carrier].unconfirmed, vec![first]);
    let resolved = regional.lineage(first, object);
    let Lineage::Successor {
      departed,
      successor,
    } = resolved
    else {
      panic!("the object has a surviving successor: {resolved:?}")
    };
    assert_eq!(departed, carrier, "recovered in the carrier's takeover");
    assert!(regional.members.contains(&successor));
    assert_eq!(
      regional.lineage(carrier, object),
      resolved,
      "a holder that saw the object under the carrier resolves the same"
    );
  }

  /// A-272, A-309: an object whose ranked successor is neither a member nor a kept retirement is settled only on that
  /// successor's completion record. Do retire an owner, then its object's ranked successor before it confirms (so it
  /// carries the owner's takeover); in one copy have the successor's survivors confirm its share, so its takeover
  /// completes while the owner's is still kept; in another drop the successor's retirement before that, as the bound
  /// does. Expect the first copy's line settled on the record (the carrier's takeover adopted the object through the
  /// owner's cohort) and the second's unresolved. Before A-272 the second read Settled, and every holder told so
  /// reclaims its copy (`takeover::resolve_held_objects`); before A-309 the first read Unresolved, since nothing kept
  /// the evidence.
  #[test]
  fn an_absent_ranked_successor_settles_the_object_only_on_its_completion_record() {
    let (mut regional, scatter) = wide_region();
    let first = HostId(1);
    regional.take_over(first, scatter);
    let object = (0..64u64)
      .map(|index| ObjectId::new(first, index))
      .find(|object| {
        regional
          .survivors_of(first)
          .contains(&successor_of(&regional, first, *object))
      })
      .expect("an object of the first owner");
    let carrier = successor_of(&regional, first, object);
    regional.take_over(carrier, scatter);
    let mut dropped = regional.clone();
    let survivors = regional.survivors_of(carrier);
    for host in &survivors {
      regional.confirm(carrier, *host);
    }
    assert!(
      !regional.retired.contains_key(&carrier) && regional.completed.contains_key(&carrier),
      "the carrier's takeover completed on its survivors' confirmations"
    );
    assert!(
      regional.retired.contains_key(&first),
      "the first owner's is kept: its survivors have not all confirmed"
    );
    assert_eq!(
      regional.lineage(first, object),
      Lineage::Settled { takeover: carrier },
      "the carrier's completed takeover adopted the object"
    );
    for member in regional.members.clone() {
      assert_eq!(
        regional.settled_for(carrier, member),
        survivors.contains(&member),
        "{member:?}: a survivor that confirmed was asked and holds a stale copy; any other member keeps its own"
      );
    }
    // The bound's drop: the carrier's retirement let go before it completed.
    dropped.retired.remove(&carrier);
    assert_eq!(
      dropped.lineage(first, object),
      Lineage::Unresolved,
      "the ranked successor is gone with no record: nothing shows the object adopted"
    );
  }

  /// A-272, A-309: a retirement the bound drops before its takeover completed leaves its objects unresolved. Do retire
  /// hosts until the region keeps fewer retirements than it has, so the bound drops the oldest with no survivor having
  /// confirmed. Expect no completion record of it and its object's lineage unresolved. Before A-272 the dropped
  /// retirement read Settled and its objects' holders reclaimed their copies.
  #[test]
  fn a_retirement_dropped_unconfirmed_by_the_bound_leaves_its_objects_unresolved() {
    let (mut regional, scatter) = wide_region();
    let first = HostId(1);
    let object = ObjectId::new(first, 3);
    regional.take_over(first, scatter);
    regional.take_over(HostId(2), scatter);
    regional.take_over(HostId(3), scatter);
    assert!(
      !regional.retired.contains_key(&first),
      "the bound dropped the oldest retirement"
    );
    assert!(
      !regional.completed.contains_key(&first),
      "a takeover dropped before it completed leaves no record"
    );
    assert_eq!(
      regional.lineage(first, object),
      Lineage::Unresolved,
      "its object is unresolved, not settled"
    );
  }

  /// §4.8 "Promotion and takeover", A-309: once every survivor has confirmed its share the retirement becomes a
  /// completion record naming them, a holder's copy of one of its objects resolves as settled (stale) on that record,
  /// and the retired host may be admitted again — never before, while its retirement still ranks successors.
  #[test]
  fn a_confirmed_takeover_settles_the_lineage_and_frees_the_id() {
    let (mut regional, scatter) = wide_region();
    let departed = HostId(1);
    regional.take_over(departed, scatter);
    let object = ObjectId::new(departed, 5);
    assert!(matches!(
      regional.lineage(departed, object),
      Lineage::Successor { .. }
    ));
    assert!(
      !regional.admits(departed),
      "a retired id is not admitted while its retirement is kept"
    );
    let survivors = regional.survivors_of(departed);
    for host in &survivors {
      regional.confirm(departed, *host);
    }
    assert!(!regional.retired.contains_key(&departed));
    assert_eq!(
      regional.completed.get(&departed),
      Some(&CompletedTakeover {
        version: regional.version,
        confirmed: survivors,
      }),
      "the completion record names every survivor, made at the last confirmation's version"
    );
    assert_eq!(
      regional.lineage(departed, object),
      Lineage::Settled { takeover: departed }
    );
    assert!(
      regional.admit(departed, None, scatter),
      "admitted once its takeover is done"
    );
    assert!(
      !regional.completed.contains_key(&departed),
      "the record of the id's earlier takeover goes with its admission: it says nothing of the member it names now"
    );
  }

  /// A-309, red before it: a takeover whose settled neighbourhood is gone is ranked among the members its joint writes
  /// reached, never settled. Do: form a region on host 1 alone, admit hosts 2 and 3 (host 1's settled neighbourhood
  /// stays itself alone until it reports the new one placed, A-234), then take host 1 over before it reports. Expect
  /// its retirement to keep the current neighbourhood {1, 2, 3}, hosts 2 and 3 its survivors, its object ranked to one
  /// of them, and no completion. Before, the retirement had no survivor, was dropped at once with nobody owing a
  /// confirmation of it, and the line read settled: hosts 2 and 3, which held the joint writes of host 1's records,
  /// would have let their copies go.
  #[test]
  fn a_bootstrap_taken_over_before_it_settled_is_ranked_among_the_members_its_joint_writes_reached()
  {
    let scatter = 3;
    let mut regional = RegionalConfiguration::formed(
      vec![HostId(1)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    assert!(regional.admit(HostId(2), None, scatter));
    assert!(regional.admit(HostId(3), None, scatter));
    regional.take_over(HostId(1), scatter);
    let object = ObjectId::new(HostId(1), 6);
    let retirement = &regional.retired[&HostId(1)];
    assert_eq!(retirement.settled.hosts, vec![HostId(1)]);
    let mut current = retirement
      .current
      .as_ref()
      .map(|current| current.hosts.clone())
      .expect("the change in flight is kept");
    current.sort_unstable_by_key(|host| host.0);
    assert_eq!(current, vec![HostId(1), HostId(2), HostId(3)]);
    assert_eq!(regional.survivors_of(HostId(1)), vec![HostId(2), HostId(3)]);
    let Lineage::Successor {
      departed,
      successor,
    } = regional.lineage(HostId(1), object)
    else {
      panic!(
        "ranked among the survivors: {:?}",
        regional.lineage(HostId(1), object)
      )
    };
    assert_eq!(departed, HostId(1));
    assert!([HostId(2), HostId(3)].contains(&successor));
    assert!(!regional.completed.contains_key(&HostId(1)));
  }

  /// A-309, red before it: a takeover carried into one whose settled neighbourhood is gone is ranked among that one's
  /// current neighbourhood, never settled. Do: in a two-host region take host 1 over, admit hosts 3 and 4, then take
  /// host 2 over before it confirms its share of host 1's takeover, its settled neighbourhood still the formation's
  /// {1, 2}. Expect hosts 3 and 4 (its current neighbourhood) its survivors, host 1's retirement kept since host 2's
  /// carries it, and both hosts' objects ranked to host 3 or 4 in host 2's takeover. Before, host 2's retirement had no
  /// survivor and was dropped at once, host 1's then had no member owing and was dropped too, and both lines read
  /// settled while hosts 3 and 4 held host 2's joint writes.
  #[test]
  fn a_takeover_carried_into_one_whose_settled_neighbourhood_is_gone_is_ranked_among_its_current_one()
   {
    let scatter = 3;
    let mut regional = RegionalConfiguration::formed(
      vec![HostId(1), HostId(2)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    regional.take_over(HostId(1), scatter);
    assert!(regional.admit(HostId(3), None, scatter));
    assert!(regional.admit(HostId(4), None, scatter));
    regional.take_over(HostId(2), scatter);
    assert_eq!(regional.retired[&HostId(2)].unconfirmed, vec![HostId(1)]);
    assert_eq!(regional.survivors_of(HostId(2)), vec![HostId(3), HostId(4)]);
    assert!(regional.retired.contains_key(&HostId(1)));
    for departed in [HostId(1), HostId(2)] {
      assert!(!regional.completed.contains_key(&departed));
      let object = ObjectId::new(departed, 2);
      assert!(
        ranked_in_round_among(
          &regional,
          departed,
          object,
          HostId(2),
          &[HostId(3), HostId(4)]
        ),
        "{departed:?}: {:?}",
        regional.lineage(departed, object)
      );
    }
  }

  /// Whether `object`'s line from `departed` is ranked, in `round`'s takeover, to one of `hosts`.
  fn ranked_in_round_among(
    regional: &RegionalConfiguration,
    departed: HostId,
    object: ObjectId,
    round: HostId,
    hosts: &[HostId],
  ) -> bool {
    matches!(
      regional.lineage(departed, object),
      Lineage::Successor { departed: by, successor } if by == round && hosts.contains(&successor)
    )
  }

  /// Banned item 8, A-309: completion records stay bounded by the members however many takeovers complete, the oldest
  /// let go first, and never one a kept retirement still needs. Do: retire host 1, then a survivor of it before it
  /// confirms (so the survivor's takeover carries host 1's), complete the survivor's takeover, then complete many
  /// transient members' takeovers while host 1's is kept; finally complete host 1's. Expect after every step at most
  /// the bound's records, the survivor's record kept among them throughout (host 1's completion rests on it), and host
  /// 1's takeover completing on it at the end, its line settled.
  #[test]
  fn completion_records_stay_bounded_and_keep_what_a_kept_retirement_needs() {
    let (mut regional, scatter) = wide_region();
    let first = HostId(1);
    regional.take_over(first, scatter);
    let carrier = regional
      .survivors_of(first)
      .first()
      .copied()
      .expect("host 1 has survivors");
    regional.take_over(carrier, scatter);
    confirm_every_survivor(&mut regional, carrier);
    assert!(regional.completed.contains_key(&carrier));
    // Shape: far more completed takeovers than the region keeps records of.
    let transients = 48;
    for index in 0..transients {
      complete_a_transient_takeover(&mut regional, HostId(100 + index), scatter);
      assert!(
        regional.completed.len() <= regional.completed_bound(),
        "bounded, the one record a kept retirement needs among them: {}",
        regional.completed.len()
      );
      assert!(
        regional.completed.contains_key(&carrier),
        "the record host 1's completion rests on is kept"
      );
    }
    assert!(regional.retired.contains_key(&first));
    confirm_every_survivor(&mut regional, first);
    assert!(regional.completed.contains_key(&first));
    assert_eq!(
      regional.lineage(first, ObjectId::new(first, 9)),
      Lineage::Settled { takeover: first }
    );
  }

  /// Confirms the share of every survivor of `departed`'s retirement.
  fn confirm_every_survivor(regional: &mut RegionalConfiguration, departed: HostId) {
    for survivor in regional.survivors_of(departed) {
      regional.confirm(departed, survivor);
    }
  }

  /// Admits `host`, retires it and confirms every survivor's share, so its takeover completes at once.
  fn complete_a_transient_takeover(
    regional: &mut RegionalConfiguration,
    host: HostId,
    scatter: u64,
  ) {
    assert!(regional.admit(host, None, scatter));
    assert!(regional.retire(host, scatter));
    confirm_every_survivor(regional, host);
  }

  /// AUD-29-26's sibling (a version names one configuration): do: a regional and a root configuration at
  /// the last version; admit, retire, take over, and every root change; expect every change refused and both
  /// configurations unchanged — on every replica alike, since the refusal reads only replicated state.
  #[test]
  fn a_configuration_at_the_last_version_refuses_every_change_unchanged() {
    // Shape: a neighbourhood of every member.
    let scatter = 5;
    let mut regional = RegionalConfiguration::formed(
      (1..=5).map(HostId).collect(),
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    regional.version = u64::MAX;
    let before = regional.clone();
    assert!(!regional.admit(HostId(9), None, scatter));
    assert!(!regional.retire(HostId(2), scatter));
    assert_eq!(regional.take_over(HostId(2), scatter), None);
    assert_eq!(regional, before);
    let mut root = RootConfiguration::formed(vec![RegionId(0), RegionId(1)]);
    root.version = u64::MAX;
    let before = root.clone();
    assert!(!root.admit_region(RegionId(2)));
    assert!(!root.retire_region(RegionId(1)));
    assert!(!root.promote_region(RegionId(1), RegionId(0)));
    assert!(!root.move_home(ObjectId::new(HostId(1), 1), RegionId(1)));
    assert_eq!(root, before);
  }

  /// AUD-29-26's sibling (an epoch fences one owner): do: take over a host whose fencing epoch is the last;
  /// expect `None` and the configuration unchanged — the old code saturated the epoch, so the dead host's
  /// records under it still passed the fence.
  #[test]
  fn a_host_at_the_last_epoch_cannot_be_taken_over() {
    // Shape: a neighbourhood of every member.
    let scatter = 5;
    let mut regional = RegionalConfiguration::formed(
      (1..=5).map(HostId).collect(),
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    regional.epochs.insert(HostId(2), HostEpoch(u64::MAX));
    let before = regional.clone();
    assert_eq!(regional.take_over(HostId(2), scatter), None);
    assert_eq!(regional, before);
  }

  /// §4.8 "Promotion and takeover", A-309: an object whose recovery cohort lost every survivor is ranked among all the
  /// retirement's survivors, so a survivor's confirmation covers it and the takeover completes only once it was
  /// adopted. A neighbourhood wider than one copyset makes the case: two of one copyset's hosts retire, and their
  /// takeovers complete, before its owner does. Before A-309 the line read lost, no round learned the object, and once
  /// every survivor confirmed the retirement was dropped and the line read settled.
  #[test]
  fn an_object_whose_cohort_has_no_survivor_is_ranked_among_the_retirements_survivors() {
    // Shape: a neighbourhood of every member, wide enough to hold two copysets of three at `f = 1`.
    let scatter = 5;
    let mut regional = RegionalConfiguration::formed(
      (1..=5).map(HostId).collect(),
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      scatter,
      false,
    );
    let owner = HostId(1);
    let object = ObjectId::new(owner, 3);
    let cohort = regional.settled[&owner].cohort(owner, object, regional.quorum);
    let others: Vec<HostId> = cohort
      .iter()
      .copied()
      .filter(|host| *host != owner)
      .collect();
    // The two retire first and their takeovers complete, so the owner carries neither into its own.
    for host in &others {
      regional.take_over(*host, scatter);
      for survivor in regional.survivors_of(*host) {
        regional.confirm(*host, survivor);
      }
      assert!(!regional.retired.contains_key(host));
      assert!(regional.completed.contains_key(host));
    }
    regional.take_over(owner, scatter);
    assert!(regional.retired[&owner].unconfirmed.is_empty());
    let survivors = regional.survivors_of(owner);
    assert!(
      !survivors.is_empty() && survivors.iter().all(|host| !cohort.contains(host)),
      "the owner's other copysets have survivors, its object's cohort none"
    );
    let resolved = regional.lineage(owner, object);
    assert!(
      matches!(resolved, Lineage::Successor { departed, successor }
        if departed == owner && survivors.contains(&successor)),
      "{resolved:?}"
    );
  }

  /// A sample answer to a takeover page: two objects' records, a next page and a fence above the epoch.
  fn sample_host_promise() -> HostPromise {
    HostPromise {
      holder: HostId(2),
      departed: HostId(1),
      epoch: HostEpoch(4),
      generation: 9,
      entries: vec![
        (
          ObjectId::new(HostId(1), 3),
          Accepted {
            sequence: 2,
            epoch: HostEpoch(3),
            value: b"head".to_vec(),
          },
        ),
        (
          ObjectId::new(HostId(1), 8),
          Accepted {
            sequence: 0,
            epoch: HostEpoch(1),
            value: Vec::new(),
          },
        ),
      ],
      next: Some(ObjectId::new(HostId(1), 8)),
      fenced: Some(HostEpoch(6)),
      unpromised: 3,
      nothing: Vec::new(),
    }
  }

  /// §4.8 "Promotion and takeover": a takeover page request and its answer round-trip their wire forms, with
  /// and without their optional fields, and an answer binds only the request it echoes.
  #[test]
  fn a_takeover_page_round_trips_and_binds_its_request() {
    let prepare = HostPrepare {
      departed: HostId(1),
      owner: HostId(3),
      epoch: HostEpoch(4),
      generation: 9,
      after: Some(ObjectId::new(HostId(1), 3)),
      named: Vec::new(),
    };
    let first_page = HostPrepare {
      after: None,
      ..prepare.clone()
    };
    for request in [prepare.clone(), first_page] {
      assert_eq!(HostPrepare::decode(&request.encode()), Ok(request));
    }
    let answer = sample_host_promise();
    assert_eq!(HostPromise::decode(&answer.encode()), Ok(answer.clone()));
    let bare = HostPromise {
      entries: Vec::new(),
      next: None,
      fenced: None,
      ..answer.clone()
    };
    assert_eq!(HostPromise::decode(&bare.encode()), Ok(bare));
    assert!(answer.binds(&prepare));
    assert!(!answer.binds(&HostPrepare {
      epoch: HostEpoch(5),
      ..prepare.clone()
    }));
    assert!(!answer.binds(&HostPrepare {
      departed: HostId(7),
      ..prepare.clone()
    }));
    assert_eq!(
      answer.encode().len(),
      HOST_PROMISE_HEADER_BYTES
        + answer
          .entries
          .iter()
          .map(|(_, accepted)| HostPromise::entry_bytes(accepted))
          .sum::<usize>(),
      "the paging arithmetic counts exactly what the encoding writes"
    );
  }

  /// Hostile input: a takeover request or answer cut short at any length, carrying trailing bytes, an unknown
  /// presence byte, or an entry count beyond what its bytes could hold decodes to a typed refusal, never a
  /// panic or an over-read.
  #[test]
  fn a_hostile_takeover_page_is_refused() {
    let request = HostPrepare {
      departed: HostId(1),
      owner: HostId(3),
      epoch: HostEpoch(4),
      generation: 9,
      after: Some(ObjectId::new(HostId(1), 3)),
      named: Vec::new(),
    }
    .encode();
    let answer = sample_host_promise().encode();
    for cut in 0..request.len() {
      assert_eq!(
        HostPrepare::decode(&request[..cut]),
        Err(RegisterError::MalformedRecord)
      );
    }
    for cut in 0..answer.len() {
      assert_eq!(
        HostPromise::decode(&answer[..cut]),
        Err(RegisterError::MalformedRecord)
      );
    }
    let mut trailing = answer.clone();
    trailing.push(0);
    assert_eq!(
      HostPromise::decode(&trailing),
      Err(RegisterError::MalformedRecord)
    );
    let mut flag = request.clone();
    flag[4 * size_of::<u64>()] = 2;
    assert_eq!(
      HostPrepare::decode(&flag),
      Err(RegisterError::MalformedRecord)
    );
    // The entry count sits after the header words, the next page (presence byte and object), the fence (presence byte
    // and epoch) and the unpromised count; claim every count there is.
    let count_at =
      4 * size_of::<u64>() + 1 + OBJECT_BYTES + 1 + size_of::<u64>() + size_of::<u32>();
    let mut lying = answer.clone();
    lying[count_at..count_at + size_of::<u32>()].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      HostPromise::decode(&lying),
      Err(RegisterError::MalformedRecord)
    );
  }

  /// A-251 (b): a by-name question and its answer round-trip with their appended lists, and a page request or answer
  /// with none keeps exactly the bytes it had before A-251, so a holder or successor that predates it reads them as
  /// before. Do: encode a request naming two objects and an answer holding one and naming one held nothing of; encode
  /// both with empty lists and compare with the fields alone. Expect: equal round trips; the empty lists add no bytes.
  #[test]
  fn a_named_question_and_its_answer_round_trip_and_a_page_keeps_its_bytes() {
    let named = HostPrepare {
      departed: HostId(1),
      owner: HostId(3),
      epoch: HostEpoch(4),
      generation: 9,
      after: None,
      named: vec![ObjectId::new(HostId(1), 3), ObjectId::new(HostId(1), 8)],
    };
    assert_eq!(HostPrepare::decode(&named.encode()), Ok(named.clone()));
    assert_eq!(
      named.encode().len(),
      4 * size_of::<u64>() + 1 + OBJECT_LIST_HEADER_BYTES + 2 * OBJECT_BYTES
    );
    let page = HostPrepare {
      named: Vec::new(),
      ..named
    };
    assert_eq!(
      page.encode().len(),
      4 * size_of::<u64>() + 1,
      "a page request's bytes are unchanged"
    );
    let answer = HostPromise {
      nothing: vec![ObjectId::new(HostId(1), 9)],
      ..sample_host_promise()
    };
    assert_eq!(HostPromise::decode(&answer.encode()), Ok(answer.clone()));
    assert_eq!(
      answer.encode().len(),
      sample_host_promise().encode().len() + OBJECT_LIST_HEADER_BYTES + OBJECT_BYTES,
      "an answer without the list keeps its bytes"
    );
  }

  /// Hostile input on A-251's appended lists: a list cut anywhere, a count of zero (which no encoder writes, so a list
  /// has one encoding), a count beyond what its bytes hold, and a stray byte after it are typed refusals, on the
  /// question and on the answer.
  #[test]
  fn a_hostile_named_list_is_refused() {
    let question = HostPrepare {
      departed: HostId(1),
      owner: HostId(3),
      epoch: HostEpoch(4),
      generation: 9,
      after: None,
      named: vec![ObjectId::new(HostId(1), 3), ObjectId::new(HostId(1), 8)],
    }
    .encode();
    let answer = HostPromise {
      nothing: vec![ObjectId::new(HostId(1), 9), ObjectId::new(HostId(1), 10)],
      ..sample_host_promise()
    }
    .encode();
    let question_list = 4 * size_of::<u64>() + 1;
    let answer_list = sample_host_promise().encode().len();
    for (bytes, list_at, decodes) in [
      (
        &question,
        question_list,
        (|b: &[u8]| HostPrepare::decode(b).is_ok()) as fn(&[u8]) -> bool,
      ),
      (&answer, answer_list, |b: &[u8]| {
        HostPromise::decode(b).is_ok()
      }),
    ] {
      for cut in list_at + 1..bytes.len() {
        assert!(!decodes(&bytes[..cut]), "cut at {cut}");
      }
      let mut zero = bytes[..list_at].to_vec();
      zero.extend_from_slice(&0u32.to_le_bytes());
      assert!(!decodes(&zero), "a count of zero");
      let mut lying = bytes.clone();
      lying[list_at..list_at + OBJECT_LIST_HEADER_BYTES].copy_from_slice(&u32::MAX.to_le_bytes());
      assert!(!decodes(&lying), "a count beyond the bytes");
      let mut trailing = bytes.clone();
      trailing.push(0);
      assert!(!decodes(&trailing), "a stray byte after the list");
    }
  }

  /// An acceptor with two accepted records, moved from a retired owner, for the persisted-state tests (A-251).
  fn moved_acceptor() -> Acceptor {
    let generation = 9;
    let mut acceptor = Acceptor::new(
      HostId(5),
      Authority {
        generation,
        owner: HostId(1),
      },
    );
    let object = ObjectId::new(HostId(1), 3);
    for (sequence, value) in [(1, b"first".to_vec()), (2, b"second".to_vec())] {
      acceptor
        .accept(&Record {
          object,
          sequence,
          epoch: FIRST_EPOCH,
          generation,
          owner: HostId(1),
          value,
        })
        .unwrap();
    }
    acceptor
      .move_authority(
        Authority {
          generation,
          owner: HostId(2),
        },
        true,
      )
      .unwrap();
    acceptor
  }

  /// A-251: an acceptor's persisted state round-trips through its encoding whole, its moved-from owner too, and the
  /// encoding is canonical. Do: encode a moved acceptor's state, decode it, rebuild, and encode again. Expect: the same
  /// authority, moved-from owner, fence and positions, and the same bytes.
  #[test]
  fn an_acceptors_persisted_state_round_trips_through_its_encoding() {
    use slates_wire::Wire;
    let acceptor = moved_acceptor();
    let bytes = acceptor.to_persisted().to_bytes();
    let rebuilt =
      Acceptor::from_persisted(HostId(5), PersistedAcceptor::from_bytes(&bytes).unwrap());
    assert_eq!(rebuilt.authority(), acceptor.authority());
    assert_eq!(rebuilt.moved_from(), Some(HostId(1)));
    assert_eq!(rebuilt.persisted(), acceptor.persisted());
    assert_eq!(rebuilt.to_persisted().to_bytes(), bytes);
  }

  /// Hostile input on an acceptor's persisted encoding (A-251; CLAUDE.md §4), its bounds: a body cut at every byte and a
  /// stray byte after it are each a typed refusal, with and without a moved-from owner. The layout before A-251 (no
  /// moved-from owner) is refused as truncated, never read as "moved from none" (§2.7).
  #[test]
  fn a_cut_or_overlong_persisted_acceptor_is_refused() {
    use slates_wire::{Wire, WireError};
    let mut acceptor = moved_acceptor();
    let moved = acceptor.to_persisted().to_bytes();
    acceptor.moved_from = None;
    let unmoved = acceptor.to_persisted().to_bytes();
    for bytes in [&moved, &unmoved] {
      for cut in 0..bytes.len() {
        assert!(
          matches!(
            PersistedAcceptor::from_bytes(&bytes[..cut]),
            Err(WireError::Truncated { .. })
          ),
          "cut at {cut}"
        );
      }
      let mut trailing = bytes.clone();
      trailing.push(0);
      assert!(matches!(
        PersistedAcceptor::from_bytes(&trailing),
        Err(WireError::TrailingBytes { count: 1 })
      ));
    }
    // The layout before A-251: the same fields without the moved-from owner's option.
    let host_bytes = HostId(0).to_bytes().len();
    let before = &moved[..moved.len() - 1 - host_bytes];
    assert!(matches!(
      PersistedAcceptor::from_bytes(before),
      Err(WireError::Truncated { .. })
    ));
  }

  /// Hostile input on an acceptor's persisted encoding (A-251; CLAUDE.md §4), its contents: an accepted count and a
  /// record length of `u32::MAX` and a foreign tag where the moved-from owner's option is are each a typed refusal, and
  /// no single flipped bit panics: it is refused or decodes to a state that rebuilds and encodes back to the flipped
  /// bytes.
  #[test]
  fn a_hostile_persisted_acceptor_is_refused() {
    use slates_wire::{Wire, WireError};
    let moved = moved_acceptor().to_persisted().to_bytes();
    let host_bytes = HostId(0).to_bytes().len();
    // Lengths past what arrived: the accepted count, then the first record's value length.
    let count_at = host_bytes + 2 * size_of::<u64>();
    let value_length_at = count_at + size_of::<u32>() + OBJECT_BYTES + 2 * size_of::<u64>();
    for at in [count_at, value_length_at] {
      let mut lying = moved.clone();
      lying[at..at + size_of::<u32>()].copy_from_slice(&u32::MAX.to_le_bytes());
      assert!(
        matches!(
          PersistedAcceptor::from_bytes(&lying),
          Err(WireError::Truncated { .. })
        ),
        "a length of u32::MAX at {at}"
      );
    }
    let mut foreign = moved.clone();
    let tag_at = moved.len() - 1 - host_bytes;
    foreign[tag_at] = slates_wire::codec::OPTION_SOME + 1;
    assert!(matches!(
      PersistedAcceptor::from_bytes(&foreign),
      Err(WireError::BadTag { .. })
    ));
    for bit in 0..moved.len() * 8 {
      let mut flipped = moved.clone();
      flipped[bit / 8] ^= 1 << (bit % 8);
      if let Ok(decoded) = PersistedAcceptor::from_bytes(&flipped) {
        let rebuilt = Acceptor::from_persisted(HostId(5), decoded.clone());
        assert_eq!(decoded.to_bytes(), flipped, "bit {bit} decodes canonically");
        assert_eq!(rebuilt.moved_from(), decoded.moved_from, "bit {bit}");
      }
    }
  }

  /// Banned item 8: retirements nobody confirms stay bounded by the members, the oldest dropped first, however
  /// many transient members come and go.
  #[test]
  fn retirements_stay_bounded_by_the_members() {
    let (mut regional, scatter) = wide_region();
    // Shape: far more transient members than the region holds.
    let transients = 64;
    for index in 0..transients {
      let host = HostId(100 + index);
      assert!(regional.admit(host, None, scatter));
      assert!(regional.retire(host, scatter));
      assert!(regional.retired.len() <= regional.retirement_bound());
    }
    let newest = HostId(100 + transients - 1);
    assert!(
      regional.retired.contains_key(&newest),
      "the newest retirement is always kept"
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
    let domains = std::collections::BTreeMap::new(); // unique-per-host
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
      // The rendezvous-first survivor is one of the object's candidate holders — placement and takeover
      // draw from the same copyset, so the successor always held a copy.
      assert!(
        candidates_for(owner, &neigh, &domains, object, quorum).contains(&first),
        "the successor is a candidate holder of the object"
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

  /// AC (§4.8, D-14 — the root configuration): region admits and retires are idempotent no-ops when already
  /// reflected, so the root log grows only for real changes.
  #[test]
  fn the_root_configuration_reconciles_region_membership() {
    let (r0, r1) = (RegionId(0), RegionId(1));
    let mut root = RootConfiguration::formed(vec![r0]);
    assert_eq!(root.version, 0);

    // Admitting a region is a real change; re-admitting it is a no-op.
    assert!(root.admit_region(r1));
    assert!(root.regions.contains(&r1));
    assert!(!root.admit_region(r1), "re-admitting a region is a no-op");

    // A clean retirement drops a region; retiring an absent one is a no-op.
    assert!(root.retire_region(r0));
    assert!(!root.regions.contains(&r0));
    assert!(
      !root.retire_region(r0),
      "retiring an absent region is a no-op"
    );
  }

  /// AC (§4.8, D-14 — the root configuration): a volume with no move is served from its creator region; a
  /// moved volume from its recorded home; a home move to a non-member region, or one already in place, is a
  /// no-op.
  #[test]
  fn the_root_configuration_homes_a_moved_volume() {
    let (r0, r1) = (RegionId(0), RegionId(1));
    let volume = ObjectId::new(HostId(1), 7);
    let mut root = RootConfiguration::formed(vec![r0, r1]);

    // A volume with no move is homed in its creator region.
    assert_eq!(root.home_of(volume, r0), r0);

    // Moving the volume's home to r1 changes where it is served; the same move again is a no-op.
    assert!(root.move_home(volume, r1));
    assert_eq!(root.home_of(volume, r0), r1, "served from its moved home");
    assert!(
      !root.move_home(volume, r1),
      "the same home again is a no-op"
    );

    // A home move to a non-member region is refused (no such region to serve from).
    assert!(!root.move_home(volume, RegionId(9)));
  }

  /// AC (§4.8 "region loss promotes the mirror through the root group"): promoting a lost region to its
  /// mirror drops the lost region from the membership and routes a volume homed there to the mirror; the
  /// promotion is followed to a fixed point across a chain, and self-/non-member promotions are refused.
  #[test]
  fn a_promoted_region_serves_the_lost_regions_volumes() {
    let (r0, r1, r2) = (RegionId(0), RegionId(1), RegionId(2));
    let volume = ObjectId::new(HostId(1), 7); // created in region r0
    let mut root = RootConfiguration::formed(vec![r0, r1, r2]);

    // r0 is lost; r1 is promoted to serve it.
    assert!(root.promote_region(r0, r1));
    assert!(
      !root.regions.contains(&r0),
      "the lost region drops from membership"
    );
    assert_eq!(
      root.home_of(volume, r0),
      r1,
      "a volume homed in the lost region is served from its promotion"
    );

    // A region cannot be promoted to itself, and a non-member cannot be promoted.
    assert!(!root.promote_region(r1, r1));
    assert!(!root.promote_region(r0, r2), "r0 is no longer a member");

    // A chained promotion (r1 now lost, promoted to r2) follows to the fixed point.
    assert!(root.promote_region(r1, r2));
    assert_eq!(
      root.home_of(volume, r0),
      r2,
      "the promotion chain r0 -> r1 -> r2 resolves to the surviving region"
    );
  }
}
