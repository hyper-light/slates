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

  /// Whether the region commit holds (`f + 1` regional acknowledgements).
  pub fn placed(&self, quorum: Quorum) -> bool {
    quorum.committed(self.acked.len())
  }

  /// Whether the mirror commit holds.
  pub fn placed_mirror(&self, quorum: Quorum) -> bool {
    self
      .mirror_acked
      .as_ref()
      .is_some_and(|m| quorum.committed(m.len()))
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

/// A register record on its way to the candidate holders (§4.8 "records are sent to all candidates"):
/// the object it belongs to, the owner's host epoch (which a holder fences against, D-16), and the
/// value bytes (the head, chain version, lease or catalog entry — opaque here). This is the canonical
/// serialization that rides the session plane's RPC to each holder (`docs/wip/fleet-transport.md` §8);
/// the format is the to-ratify recommendation (object, epoch, then a length-prefixed value).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
  /// The object (volume head, chain, lease, catalog entry) this record belongs to.
  pub object: u64,
  /// The owner's host epoch; a holder refuses a record below the highest epoch it has seen (fencing).
  pub epoch: HostEpoch,
  /// The record's value bytes (opaque to the register protocol).
  pub value: Vec<u8>,
}

/// The fixed prefix of an encoded record: the object and epoch words plus the value's length.
/// Format: §4.8 record layout — `object` (u64 LE), `epoch` (u64 LE), `value_len` (u32 LE), then the
/// value bytes; a record is MTU-shippable, so the length is a `u32`.
const RECORD_PREFIX_BYTES: usize = size_of::<u64>() + size_of::<u64>() + size_of::<u32>();

impl Record {
  /// The canonical bytes: object, epoch, value length, value — little-endian throughout, so two hosts
  /// encode a record identically (a determinism the holder set relies on).
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::with_capacity(RECORD_PREFIX_BYTES + self.value.len());
    out.extend_from_slice(&self.object.to_le_bytes());
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
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
    let (object_bytes, rest) = bytes.split_at(size_of::<u64>());
    let (epoch_bytes, rest) = rest.split_at(size_of::<u64>());
    let (len_bytes, value_bytes) = rest.split_at(size_of::<u32>());
    let object = u64::from_le_bytes(object_bytes.try_into().unwrap_or([0; 8]));
    let epoch = HostEpoch(u64::from_le_bytes(epoch_bytes.try_into().unwrap_or([0; 8])));
    let value_len = u32::from_le_bytes(len_bytes.try_into().unwrap_or([0; 4]));
    let value_len = usize::try_from(value_len).unwrap_or(usize::MAX);
    if value_bytes.len() != value_len {
      return Err(RegisterError::MalformedRecord);
    }
    Ok(Record {
      object,
      epoch,
      value: value_bytes.to_vec(),
    })
  }
}

/// A candidate holder of an object's records (§4.8 "2f+1 candidate holders, the owner among them"). A
/// real holder is reached over the session plane's RPC; the owner's own hold is local. `deliver`
/// applies the record — fencing it against the holder's highest-seen epoch — and returns the holder's
/// id on acknowledgement, or a [`RegisterError`] (a fenced record does not acknowledge, so it is not
/// counted toward the commit). This is the seam the transport plugs a remote holder into (slice 5c);
/// the register protocol here stays sans-io.
pub trait Holder {
  /// Delivers `record` to this holder, returning its host id if it acknowledges (held under an epoch
  /// at least as high as any it has seen), or refusing.
  fn deliver(&mut self, record: &Record) -> Result<HostId, RegisterError>;
}

/// Ships `record` to the `candidates`' `holders` (the owner's local hold among them, `holders[i]` the
/// holder for `candidates[i]`) and returns the resulting [`Placement`] — the candidates and the subset
/// that acknowledged. The write commits when the placement is `placed(quorum)` (`f + 1`
/// acknowledgements, §4.8 "one quorum rule"); a holder that fences the record (a stale epoch) does not
/// acknowledge and is not counted, so a resumed stale owner cannot gather a quorum. At `f = 0` there is
/// one holder, the owner, and its local hold is the commit — the same code path, never a mode switch
/// (R8). `candidates` and `holders` must be the same length (a candidate and the holder that reaches
/// it); a surplus holder or candidate beyond the shorter is ignored.
pub fn commit_over_holders(
  candidates: &[HostId],
  record: &Record,
  holders: &mut [&mut dyn Holder],
) -> Placement {
  let mut acked = Vec::new();
  for holder in holders.iter_mut() {
    if let Ok(host) = holder.deliver(record) {
      acked.push(host);
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

  /// A simulated fleet holder that fences a record and acknowledges: the other leg of the N=1
  /// differential, so `f = 1` runs the same protocol as `f = 0` locally. It is a [`Holder`] — the
  /// same seam a real transport-backed holder plugs into (slice 5c) — so the differential exercises
  /// `commit_over_holders`, not a parallel simulation.
  struct SimHolder {
    id: HostId,
    fence: Fence,
  }

  impl SimHolder {
    fn new(id: HostId) -> SimHolder {
      SimHolder {
        id,
        fence: Fence::new(),
      }
    }
  }

  impl Holder for SimHolder {
    fn deliver(&mut self, record: &Record) -> Result<HostId, RegisterError> {
      self.fence.accept(record.epoch)?;
      Ok(self.id)
    }
  }

  /// Runs a register write over `config`'s candidates through `commit_over_holders` with a simulated
  /// holder per candidate, and returns the placement observed. The owner's holder is local; peers are
  /// simulated — the same code at f=0 (one holder) and f=1 (three).
  fn write(config: &Configuration, object: u64, epoch: HostEpoch) -> Placement {
    let candidates = candidates_for(config.owner, &config.neighbourhood, object, config.quorum);
    let record = Record {
      object,
      epoch,
      value: Vec::new(),
    };
    let mut holders: Vec<SimHolder> = candidates.iter().map(|h| SimHolder::new(*h)).collect();
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
      object: 0x0102_0304_0506_0708,
      epoch: HostEpoch(0x1122_3344),
      value: b"head@v7".to_vec(),
    };
    let bytes = record.encode();
    assert_eq!(Record::decode(&bytes), Ok(record.clone()), "round-trip");

    // Golden: object (8 LE) + epoch (8 LE) + len (4 LE) + value.
    let mut golden = vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
    golden.extend_from_slice(&[0x44, 0x33, 0x22, 0x11, 0, 0, 0, 0]);
    golden.extend_from_slice(&[0x07, 0, 0, 0]);
    golden.extend_from_slice(b"head@v7");
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
  /// the commit: at f=1 with only the owner accepting and both peers fencing, the write is **not**
  /// placed (one ack, commit needs two) — StaleNeverCommits, over the real ship-and-collect path.
  #[test]
  fn a_fencing_holder_is_not_counted_toward_the_commit() {
    let owner = HostId(1);
    let quorum = Quorum { f: 1 };
    let candidates = vec![owner, HostId(2), HostId(3)];
    let record = Record {
      object: 9,
      epoch: HostEpoch(5),
      value: b"v".to_vec(),
    };

    // The owner accepts; the two peers have already seen a higher epoch, so they fence this record.
    let mut owner_holder = SimHolder::new(owner);
    let mut peer_two = SimHolder::new(HostId(2));
    let mut peer_three = SimHolder::new(HostId(3));
    peer_two.fence.accept(HostEpoch(6)).unwrap();
    peer_three.fence.accept(HostEpoch(6)).unwrap();
    let mut refs: Vec<&mut dyn Holder> = vec![&mut owner_holder, &mut peer_two, &mut peer_three];

    let placement = commit_over_holders(&candidates, &record, &mut refs);
    assert_eq!(placement.acked, vec![owner], "only the owner acknowledged");
    assert!(
      !placement.placed(quorum),
      "one acknowledgement does not commit at f=1; the fenced peers are not counted"
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
}
