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
  pub fn local(owner: HostId, quorum: Quorum, neighbourhood: &[HostId], object: u64) -> Placement {
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
  object: u64,
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
    .map(|h| (rendezvous_weight(h.0, object), *h))
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

/// The rendezvous weight of a host for an object: a hash of the pair, so the ranking is
/// pseudo-random per object and stable across hosts (FNV-1a over the two words; the placement
/// research calls for a good mixer, and this is replaced by the measured one when placement is
/// tuned in Phase 8).
fn rendezvous_weight(host: u64, object: u64) -> u64 {
  /// Format: the FNV-1a 64-bit offset basis (Fowler, Noll, Vo).
  const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
  /// Format: the FNV-1a 64-bit prime.
  const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
  let mut hash = FNV_OFFSET;
  for byte in host.to_le_bytes().iter().chain(object.to_le_bytes().iter()) {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(FNV_PRIME);
  }
  hash
}

/// The host that rendezvous ranks first for `object` among `hosts` — the highest rendezvous weight,
/// the lowest id breaking a tie (the same total order [`candidates_for`] uses for the candidates after
/// the owner). `None` if `hosts` is empty. Takeover assigns a dead owner's object to this survivor of
/// its neighbourhood (§4.8 "Promotion and takeover"; the worked example's "rendezvous ranks first
/// among {B, C, D}").
pub fn rendezvous_first(hosts: &[HostId], object: u64) -> Option<HostId> {
  let mut ranked: Vec<(u64, HostId)> = hosts
    .iter()
    .map(|host| (rendezvous_weight(host.0, object), *host))
    .collect();
  ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
  ranked.first().map(|(_, host)| *host)
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
  pub object: u64,
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
/// Format: §4.8 record layout — `owner`, `object`, `sequence`, `epoch`, `generation` (each u64 LE),
/// `value_len` (u32 LE), then the value bytes; a record is MTU-shippable, so the length is a `u32`.
const RECORD_PREFIX_BYTES: usize = 5 * size_of::<u64>() + size_of::<u32>();

impl Record {
  /// The canonical bytes: the five header words, the value length, the value — little-endian
  /// throughout, so two hosts encode a record identically (the determinism its identity relies on).
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_PREFIX_BYTES + self.value.len());
    out.extend_from_slice(&self.owner.0.to_le_bytes());
    out.extend_from_slice(&self.object.to_le_bytes());
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
    let (object_bytes, rest) = rest.split_at(size_of::<u64>());
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
      object: word(object_bytes),
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
  pub object: u64,
  /// The ledger position accepted.
  pub sequence: u64,
  /// The generation it was accepted under.
  pub generation: u64,
  /// The accepted record's identity.
  pub identity: [u8; 32],
}

/// An acknowledgement's fixed wire size.
/// Format: four u64 header words (holder, object, sequence, generation) then the 32-byte BLAKE3
/// identity — 64 bytes.
const ACK_BYTES: usize = 4 * size_of::<u64>() + 32;

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
    out.extend_from_slice(&self.object.to_le_bytes());
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
    let (object_bytes, rest) = rest.split_at(size_of::<u64>());
    let (sequence_bytes, rest) = rest.split_at(size_of::<u64>());
    let (generation_bytes, identity_bytes) = rest.split_at(size_of::<u64>());
    let mut identity = [0u8; 32];
    identity.copy_from_slice(identity_bytes);
    Ok(Ack {
      holder: HostId(word(holder_bytes)),
      object: word(object_bytes),
      sequence: word(sequence_bytes),
      generation: word(generation_bytes),
      identity,
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
  accepted: std::collections::BTreeMap<(u64, u64), (HostEpoch, Vec<u8>)>,
}

/// A holder's durable accepted positions, for recovery: `(object, sequence, epoch, value)` per entry.
pub type AcceptedPositions = Vec<(u64, u64, HostEpoch, Vec<u8>)>;

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
  pub fn place(&self, object: u64) -> Placement {
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
      object,
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
    let candidates = candidates_for(config.owner, &config.neighbourhood, object, config.quorum);
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
      object: 2,
      sequence: 3,
      epoch: HostEpoch(4),
      generation: 5,
      value: b"v".to_vec(),
    };
    let bytes = record.encode();
    assert_eq!(Record::decode(&bytes), Ok(record.clone()), "round-trip");

    // Golden: owner, object, sequence, epoch, generation (each 8 LE), value_len (4 LE), value.
    let mut golden = Vec::new();
    for word in [1u64, 2, 3, 4, 5] {
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
      object: 9,
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
      object: 9,
      sequence: 0,
      epoch: HostEpoch(5),
      generation,
      value: b"v".to_vec(),
    };
    let good_ack = Ack {
      holder: owner,
      object: 9,
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
      object: 1,
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
      object: 1,
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
      object: 5,
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
    let placement = config.place(42);
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
    for object in 0..256u64 {
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
      rendezvous_first(&[], 42),
      None,
      "an empty survivor set has no successor"
    );

    let owner = HostId(1);
    let neigh = vec![owner, HostId(2), HostId(3), HostId(4), HostId(5)];
    let survivors: Vec<HostId> = neigh.iter().copied().filter(|h| *h != owner).collect();
    let quorum = Quorum { f: 2 };
    let mut winners = std::collections::BTreeSet::new();
    for object in 0..256u64 {
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
}
