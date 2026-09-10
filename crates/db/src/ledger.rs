//! The fenced ledger register: a register over time (§4.8 "Promotion and takeover"; §4.16
//! "Commit"; D-27, D-14, D-16). A register is not one value but a growing log. The owner is the
//! distinguished proposer of its object's register under its host epoch (Vertical Paxos II with
//! the leader among the acceptors): it appends a record at the next sequence to the object's
//! `2f + 1` candidate holders, and the record commits the moment `f + 1` of them acknowledge it.
//! §4.16's merge records are entries in such a log; a holder recomputes the verdict from the
//! committed log before serving. This module is the protocol over that log, built on the register
//! primitives of [`crate::register`] — the [`Quorum`] (`2f + 1` candidates, commit at `f + 1`),
//! the per-holder [`Fence`] (the highest host epoch a holder has accepted a record under), and the
//! rendezvous [`candidates_for`] placement.
//!
//! It is a pure, deterministic simulation (R8): a holder is in-memory state and a message is a
//! direct call, so one body of code is the laptop (`f = 0`: one holder, a commit of one, the local
//! append) and the fleet (`f > 0`: `2f + 1` holders, a commit of `f + 1`) with no network, no
//! clock, no mode switch. The simulation is the protocol's proof — it drives proposals, partitions
//! and takeovers and checks the properties the `FencedRegister` TLA+ model proved:
//!
//! - **Agreement / no divergent quorum**: no two distinct records are each acknowledged by a quorum
//!   at one position; a position has at most one committed value.
//! - **TotalOrder**: the committed records are a dense prefix `0..k`; every holder that holds a
//!   committed position holds the committed value there once repaired.
//! - **StaleNeverCommits**: once a takeover has fenced a quorum, the superseded owner reaches at
//!   most `f` holders and so can never commit another record.
//! - **Continuity / NoLoss**: a new owner's phase-one read of a quorum intersects the commit quorum
//!   of every committed record, so the log it adopts begins with every record that had committed.
//!
//! The subtlety the model exposed and this code embodies: a record acknowledged by fewer than
//! `f + 1` holders before a takeover has not committed. The new owner adopts, per position, the
//! record carried under the highest epoch it read; a committed record was seen by the read quorum
//! and no later owner ever proposed a different record at a committed position, so the highest-epoch
//! record at a committed position is the committed one. A record that was never committed may be
//! adopted or dropped — either is safe, because the superseded owner that proposed it is fenced and
//! no reader ever saw it commit. This is the Vertical Paxos II recovery round, one per register.
//!
//! Ownership by handle, sharing by move (no `Arc`): the [`Cohort`] owns its holders in a map keyed
//! by host id; an [`Owner`] is a small proposer view (its id, its epoch, its log) that acts against
//! a cohort by `&mut`. A takeover produces a fresh `Owner` by value; the superseded one is simply
//! kept and continues to be refused, which is how [`Owner::propose`] models `StaleNeverCommits`.

use std::collections::BTreeMap;

use crate::register::{
  Authority, FIRST_EPOCH, Fence, HostEpoch, HostId, ObjectId, Prepare, Quorum, RegisterError,
  candidates_for,
};

/// One entry in a holder's log. Its position is its index in the log (dense, `0`-based); it carries
/// the payload's identity (the `blake3` of a merge record's declared work, a head, a lease — the
/// register class does not change the protocol) and the host epoch it was last accepted under, so a
/// phase-one read can pick the freshest record at each position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
  /// The host epoch the record was accepted under.
  pub epoch: HostEpoch,
  /// The payload's identity; the body itself lives in content storage.
  pub identity: [u8; 32],
}

/// A candidate holder's state for one register: its [`Fence`] (the highest epoch it has accepted a
/// record under) and its log, dense by position. In the fleet these live on `2f + 1` hosts; here
/// they are in-memory, one per candidate.
#[derive(Clone, Debug, Default)]
struct Holder {
  fence: Fence,
  log: Vec<Record>,
}

impl Holder {
  /// Brings the holder into agreement with `log` under `epoch`, the fence already raised by the
  /// caller. The agreeing prefix is skipped; from the first divergence the holder's entries are
  /// overwritten or appended under `epoch`; a stale tail beyond `log` (uncommitted records from a
  /// superseded owner, always under a lower epoch) is dropped. Overwriting is sound: the fence was
  /// raised to `epoch` first, so `epoch` is at least every epoch already stored here.
  ///
  /// Every position's accepted epoch is refreshed to `epoch`, including positions whose identity is
  /// unchanged. This is load-bearing, not an inefficiency to skip: a holder accepts the whole
  /// offered log under the owner's current epoch, so a record re-committed by a new owner must carry
  /// that new epoch. Skipping the matching prefix (leaving a committed record at its original low
  /// epoch) let a later phase-one `adopt` — which picks the highest epoch per position — prefer a
  /// stale-but-higher-epoch value on another holder and overwrite the committed one (the source
  /// audit's BUG-12, 2026-09-05: a NoLoss/TotalOrder violation).
  fn reconcile(&mut self, epoch: HostEpoch, log: &[[u8; 32]]) {
    for (position, identity) in log.iter().enumerate() {
      let record = Record {
        epoch,
        identity: *identity,
      };
      if position < self.log.len() {
        self.log[position] = record;
      } else {
        self.log.push(record);
      }
    }
    self.log.truncate(log.len());
  }
}

/// The `2f + 1` candidate holders for one object's register, keyed by host id, with the quorum the
/// fault-domain tree fixed. The cohort accepts records offered by an owner and answers what a
/// position has committed to; it never proposes.
#[derive(Clone, Debug)]
pub struct Cohort {
  quorum: Quorum,
  candidates: Vec<HostId>,
  holders: BTreeMap<HostId, Holder>,
}

impl Cohort {
  /// The cohort for `object`: the rendezvous candidates from the owner's neighbourhood (owner
  /// first, `2f + 1` total), each an empty holder at the first epoch. At `f = 0` this is the owner
  /// alone.
  pub fn new(owner: HostId, neighbourhood: &[HostId], object: ObjectId, quorum: Quorum) -> Cohort {
    let candidates = candidates_for(owner, neighbourhood, object, quorum);
    let mut holders = BTreeMap::new();
    for candidate in &candidates {
      holders.insert(*candidate, Holder::default());
    }
    Cohort {
      quorum,
      candidates,
      holders,
    }
  }

  /// The quorum this cohort commits under.
  pub fn quorum(&self) -> Quorum {
    self.quorum
  }

  /// The candidate host ids, owner first.
  pub fn candidates(&self) -> &[HostId] {
    &self.candidates
  }

  /// The highest epoch any holder has accepted under; a takeover's new epoch is its successor.
  fn highest_epoch(&self) -> u64 {
    self
      .holders
      .values()
      .map(|holder| holder.fence.seen.0)
      .max()
      .unwrap_or(FIRST_EPOCH.0)
  }

  /// Offers `log` (the owner's whole log) to every reachable candidate under `epoch`, reconciling
  /// each holder that accepts and returning the holders that now hold the log's last position — the
  /// acknowledging set for the newest record. A holder fenced above `epoch` (a takeover has
  /// happened) refuses and is not counted, which is how a superseded owner fails to commit.
  fn offer(&mut self, epoch: HostEpoch, log: &[[u8; 32]], reachable: &Reach) -> Vec<HostId> {
    let mut acked = Vec::new();
    if log.is_empty() {
      return acked;
    }
    for candidate in &self.candidates {
      if !reachable.can_reach(*candidate) {
        continue;
      }
      let Some(holder) = self.holders.get_mut(candidate) else {
        continue;
      };
      if holder.fence.accept(epoch).is_err() {
        continue;
      }
      holder.reconcile(epoch, log);
      if holder.log.len() >= log.len() {
        acked.push(*candidate);
      }
    }
    acked
  }

  /// The identity a quorum agrees on at `position`, if any. The oracle for what has committed.
  pub fn committed_at(&self, position: usize) -> Option<[u8; 32]> {
    let mut counts: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    for holder in self.holders.values() {
      if let Some(record) = holder.log.get(position) {
        let count = counts.entry(record.identity).or_insert(0);
        *count += 1;
        if self.quorum.committed(*count) {
          return Some(record.identity);
        }
      }
    }
    None
  }

  /// Whether two distinct identities are each held by a quorum at `position` — the Agreement
  /// violation that must never occur. Used by the invariant checks.
  pub fn diverges_at(&self, position: usize) -> bool {
    let mut counts: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    for holder in self.holders.values() {
      if let Some(record) = holder.log.get(position) {
        *counts.entry(record.identity).or_insert(0) += 1;
      }
    }
    counts
      .values()
      .filter(|count| self.quorum.committed(**count))
      .count()
      > 1
  }

  /// The committed prefix: the identities of positions `0..k`, `k` the first position no quorum
  /// agrees on. A dense prefix by TotalOrder.
  pub fn committed_prefix(&self) -> Vec<[u8; 32]> {
    let mut prefix = Vec::new();
    let mut position = 0usize;
    while let Some(identity) = self.committed_at(position) {
      prefix.push(identity);
      position += 1;
    }
    prefix
  }
}

/// Which candidates a message can reach — the partition model. `all` reaches every candidate; a
/// set reaches only its members. A takeover or a commit needs a quorum reachable.
#[derive(Clone, Debug)]
pub enum Reach {
  /// Every candidate is reachable.
  All,
  /// Only these hosts are reachable.
  Only(std::collections::BTreeSet<HostId>),
}

impl Reach {
  /// Whether `host` is reachable.
  fn can_reach(&self, host: HostId) -> bool {
    match self {
      Reach::All => true,
      Reach::Only(set) => set.contains(&host),
    }
  }
}

/// The outcome of a proposal: the position offered, the holders that acknowledged, and whether that
/// reached the commit quorum. A superseded owner sees `committed = false` because the fenced
/// holders refused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
  /// The position the record was offered at.
  pub position: u64,
  /// The holders that acknowledged.
  pub acked: Vec<HostId>,
  /// Whether the acknowledgements reached `f + 1`.
  pub committed: bool,
}

/// Why a takeover could not proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverError {
  /// Fewer than `f + 1` candidates were reachable, so the phase-one round could not fence and read
  /// a quorum; the takeover is refused rather than splitting the register.
  NoQuorum {
    /// The candidates that were reachable.
    reachable: usize,
    /// The candidates a quorum needs.
    needed: usize,
  },
}

/// A proposer's view of one register: its host id, the epoch it proposes under, and its log (the
/// records it believes the register holds, its own proposals plus what it adopted at takeover). An
/// owner acts against a [`Cohort`]; sharing is by this small value, never a lock.
#[derive(Clone, Debug)]
pub struct Owner {
  /// The proposing host.
  pub id: HostId,
  /// The host epoch it proposes under.
  pub epoch: HostEpoch,
  log: Vec<[u8; 32]>,
}

impl Owner {
  /// The first owner of a fresh register: the cohort's owner (its first candidate) at the first
  /// epoch, with an empty log.
  pub fn bootstrap(cohort: &Cohort) -> Owner {
    let id = cohort.candidates.first().copied().unwrap_or(HostId(0));
    Owner {
      id,
      epoch: FIRST_EPOCH,
      log: Vec::new(),
    }
  }

  /// The owner's current log length (the number of records it has proposed or adopted).
  pub fn len(&self) -> usize {
    self.log.len()
  }

  /// Whether the owner's log is empty.
  pub fn is_empty(&self) -> bool {
    self.log.is_empty()
  }

  /// Proposes a record carrying `identity` at the next position, offering the whole log to the
  /// reachable holders under this owner's epoch. Commits when `f + 1` acknowledge. A superseded
  /// owner (its epoch below the holders' fences) reaches at most `f` holders and does not commit;
  /// its local log still grows, but nothing it writes is ever a quorum.
  pub fn propose(&mut self, cohort: &mut Cohort, identity: [u8; 32], reachable: &Reach) -> Commit {
    self.log.push(identity);
    let position = (self.log.len() - 1) as u64;
    let acked = cohort.offer(self.epoch, &self.log, reachable);
    let committed = cohort.quorum().committed(acked.len());
    Commit {
      position,
      acked,
      committed,
    }
  }

  /// Replicates `log` onto the reachable holders under this owner's epoch and returns the committed
  /// prefix length. `log` must extend this owner's current log — the same values at every position
  /// it already holds — so offering it whole catches up a returned holder and appends the new tail
  /// without ever rewriting a committed position (the values agree there). It is idempotent and
  /// resumable: re-offering the same log to a newly reachable holder simply completes it. The mirror
  /// shipper replays the home region's committed prefix this way, and the healer brings a lagging
  /// holder current, neither proposing a new record.
  pub fn replicate(&mut self, cohort: &mut Cohort, log: &[[u8; 32]], reachable: &Reach) -> usize {
    self.log = log.to_vec();
    let _ = cohort.offer(self.epoch, &self.log, reachable);
    cohort.committed_prefix().len()
  }

  /// Takes over the register as `new_id`: fences and reads a reachable quorum (phase one), then
  /// adopts, per position, the record carried under the highest epoch read. The new epoch is the
  /// successor of the highest any holder has seen, so it fences the superseded owner on the quorum
  /// it read. Refused if fewer than `f + 1` candidates are reachable. The returned owner's log
  /// begins with every record that had committed (Continuity), because the read quorum intersects
  /// every commit quorum.
  pub fn take_over(
    cohort: &mut Cohort,
    new_id: HostId,
    reachable: &Reach,
  ) -> Result<Owner, TakeoverError> {
    let reachable_candidates: Vec<HostId> = cohort
      .candidates
      .iter()
      .copied()
      .filter(|candidate| reachable.can_reach(*candidate))
      .collect();
    let needed = cohort.quorum().commit();
    if reachable_candidates.len() < needed {
      return Err(TakeoverError::NoQuorum {
        reachable: reachable_candidates.len(),
        needed,
      });
    }
    // The next epoch fences every holder the round reaches; it is the successor of the highest
    // epoch seen, not a tuning constant (structural increment, as `2f+1` and `f+1` are).
    let new_epoch = HostEpoch(cohort.highest_epoch().saturating_add(1));
    let mut logs: Vec<Vec<Record>> = Vec::new();
    for candidate in &reachable_candidates {
      if let Some(holder) = cohort.holders.get_mut(candidate) {
        // Raising the fence to a strictly higher epoch always succeeds; it fences the old owner.
        let _ = holder.fence.accept(new_epoch);
        logs.push(holder.log.clone());
      }
    }
    Ok(Owner {
      id: new_id,
      epoch: new_epoch,
      log: adopt(&logs),
    })
  }
}

/// Adopts a log from the phase-one read: at each position, the identity carried under the highest
/// epoch any read holder holds there. The read logs are dense, so every position below the longest
/// has a record; the result is dense. A committed position's only identity is the committed one (no
/// later owner proposes a different record at a committed position), so its highest-epoch record is
/// the committed value.
///
/// Public so the cluster plane's live ledger promotion adopts a quorum of [`LedgerPromise`] logs by the
/// exact same rule the sans-io [`Owner::take_over`] does — the transport gathers the holders' logs over
/// the wire, this folds them, and the two must agree (the sim is the oracle).
pub fn adopt(logs: &[Vec<Record>]) -> Vec<[u8; 32]> {
  let max_len = logs.iter().map(Vec::len).max().unwrap_or(0);
  let mut adopted = Vec::with_capacity(max_len);
  for position in 0..max_len {
    let mut best: Option<Record> = None;
    for log in logs {
      if let Some(record) = log.get(position) {
        best = match best {
          Some(current) if current.epoch >= record.epoch => Some(current),
          _ => Some(*record),
        };
      }
    }
    if let Some(record) = best {
      adopted.push(record.identity);
    }
  }
  adopted
}

/// A candidate holder's reply to a phase-one [`Prepare`] over the transport for a *ledger* register — the
/// multi-entry counterpart of [`crate::register::Promise`], which reports a single highest record. It
/// names the promising holder, the object, the epoch it promised (echoing the prepare so the reply binds
/// to it), the generation, and its **whole log**: every position, each a [`Record`] carrying the epoch it
/// was last accepted under, so the new owner adopts, per position, the record under the highest epoch
/// across a phase-one quorum ([`adopt`]). A holder counts toward the quorum only if its promise
/// [`binds`](LedgerPromise::binds) to the prepare and comes from a distinct candidate, so a fenced,
/// foreign or wrong-object reply cannot manufacture a promotion quorum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerPromise {
  /// The holder that raised its fence and reports its log.
  pub holder: HostId,
  /// The object the promise is for.
  pub object: ObjectId,
  /// The epoch the holder promised (echoes the prepare's epoch).
  pub epoch: HostEpoch,
  /// The generation the holder is serving under.
  pub generation: u64,
  /// The holder's whole log, dense by position; each entry an identity and the epoch it is held under.
  pub log: Vec<Record>,
}

/// A ledger promise's fixed prefix on the wire: holder (u64 LE), object (16 bytes), epoch and generation
/// (each u64 LE), then the log length (u32 LE).
/// Format: the §4.8 promotion reply generalised to a whole log; the length is a `u32` because the log
/// ships in one MTU-bounded transport frame, whose cap bounds it — no separate constant is invented.
const LEDGER_PROMISE_PREFIX_BYTES: usize = 3 * size_of::<u64>() + OBJECT_BYTES + size_of::<u32>();
/// One log entry's wire size: its epoch (u64 LE) then its 32-byte identity.
/// Format: the ledger [`Record`] is `{ epoch, identity }`, the identity a 32-byte blake3.
const LEDGER_ENTRY_BYTES: usize = size_of::<u64>() + IDENTITY_BYTES;
/// The object id's wire width (bytes).
/// Format: [`ObjectId`] is a 128-bit id (D-14: the high half is the creator host).
const OBJECT_BYTES: usize = 16;
/// A payload identity's wire width (bytes).
/// Format: a blake3 digest is 32 bytes.
const IDENTITY_BYTES: usize = 32;

impl LedgerPromise {
  /// Whether this promise answers `prepare` — the object, epoch and generation all match, so a reply for
  /// a different prepare cannot be counted for this one.
  pub fn binds(&self, prepare: &Prepare) -> bool {
    self.object == prepare.object
      && self.epoch == prepare.epoch
      && self.generation == prepare.generation
  }

  /// The canonical bytes: the header words, the log length, then each entry's epoch and identity.
  pub fn encode(&self) -> Vec<u8> {
    let mut out =
      Vec::with_capacity(LEDGER_PROMISE_PREFIX_BYTES + self.log.len() * LEDGER_ENTRY_BYTES);
    out.extend_from_slice(&self.holder.0.to_le_bytes());
    out.extend_from_slice(&self.object.0);
    out.extend_from_slice(&self.epoch.0.to_le_bytes());
    out.extend_from_slice(&self.generation.to_le_bytes());
    let count = u32::try_from(self.log.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    for record in &self.log {
      out.extend_from_slice(&record.epoch.0.to_le_bytes());
      out.extend_from_slice(&record.identity);
    }
    out
  }

  /// Reconstructs a promise from its bytes, or a typed [`RegisterError::MalformedRecord`] if they are the
  /// wrong length — the declared log length must match the bytes present *exactly*, so a message that
  /// crossed the network (hostile input) can neither over- nor under-run the buffer, and no length is
  /// trusted before it is checked against the bytes actually there.
  pub fn decode(bytes: &[u8]) -> Result<LedgerPromise, RegisterError> {
    if bytes.len() < LEDGER_PROMISE_PREFIX_BYTES {
      return Err(RegisterError::MalformedRecord);
    }
    let word = |slice: &[u8]| u64::from_le_bytes(slice.try_into().unwrap_or([0; 8]));
    let (holder_bytes, rest) = bytes.split_at(size_of::<u64>());
    let (object_bytes, rest) = rest.split_at(OBJECT_BYTES);
    let (epoch_bytes, rest) = rest.split_at(size_of::<u64>());
    let (generation_bytes, rest) = rest.split_at(size_of::<u64>());
    let (count_bytes, mut entries) = rest.split_at(size_of::<u32>());
    let count = u32::from_le_bytes(count_bytes.try_into().unwrap_or([0; 4])) as usize;
    // The declared count must match the bytes present exactly — no partial or padded entry survives.
    if entries.len() != count.saturating_mul(LEDGER_ENTRY_BYTES) {
      return Err(RegisterError::MalformedRecord);
    }
    let mut log = Vec::with_capacity(count);
    for _ in 0..count {
      let (entry, next) = entries.split_at(LEDGER_ENTRY_BYTES);
      entries = next;
      let (entry_epoch, identity_bytes) = entry.split_at(size_of::<u64>());
      let mut identity = [0u8; IDENTITY_BYTES];
      identity.copy_from_slice(identity_bytes);
      log.push(Record {
        epoch: HostEpoch(word(entry_epoch)),
        identity,
      });
    }
    let mut object = [0u8; OBJECT_BYTES];
    object.copy_from_slice(object_bytes);
    Ok(LedgerPromise {
      holder: HostId(word(holder_bytes)),
      object: ObjectId(object),
      epoch: HostEpoch(word(epoch_bytes)),
      generation: word(generation_bytes),
      log,
    })
  }
}

/// A candidate holder of one object's *ledger* register as a standalone node — the transport counterpart
/// of the in-cohort [`Holder`], the multi-entry analogue of [`crate::register::Acceptor`]. It holds the
/// object's log (dense by position) under a [`Fence`] (the highest epoch it has accepted a record under)
/// and a configuration [`Authority`], and answers a new owner's phase-one [`Prepare`] with its whole log
/// so the owner adopts the committed prefix. A takeover installs the new authority
/// ([`install_authority`](LedgerAcceptor::install_authority)) before the prepare, mirroring the register
/// plane's discipline; the serve routes a prepare to the acceptor for its object.
pub struct LedgerAcceptor {
  id: HostId,
  object: ObjectId,
  authority: Authority,
  fence: Fence,
  log: Vec<Record>,
}

impl LedgerAcceptor {
  /// A fresh acceptor for holder `id` and `object` under `authority`, holding an empty log.
  pub fn new(id: HostId, object: ObjectId, authority: Authority) -> LedgerAcceptor {
    LedgerAcceptor {
      id,
      object,
      authority,
      fence: Fence::new(),
      log: Vec::new(),
    }
  }

  /// An acceptor recovered after a restart from its promised epoch and its `log` — the committed state a
  /// real holder writes to anchor-owned RAM before acknowledging (§4.8 "persist effect and completion as
  /// one recoverable publication"), mirroring [`crate::register::Acceptor::recovered`]. The fence is
  /// raised to `promised` and the log restored, so phase one reads exactly what the holder held.
  pub fn recovered(
    id: HostId,
    object: ObjectId,
    authority: Authority,
    promised: HostEpoch,
    log: Vec<Record>,
  ) -> LedgerAcceptor {
    let mut fence = Fence::new();
    let _ = fence.accept(promised);
    LedgerAcceptor {
      id,
      object,
      authority,
      fence,
      log,
    }
  }

  /// The holder's current log.
  pub fn log(&self) -> &[Record] {
    &self.log
  }

  /// Installs a new configuration authority (the holder applying the configuration update the regional
  /// group distributed on a takeover), mirroring [`crate::register::Acceptor::install_authority`]: a
  /// generation below the current one is a stale configuration and is refused; the current or a newer one
  /// is adopted, so the new owner becomes the sole authorised writer and the old owner is fenced by
  /// generation. The log is kept — it is exactly the state phase one reads.
  pub fn install_authority(&mut self, authority: Authority) -> Result<(), RegisterError> {
    if authority.generation < self.authority.generation {
      return Err(RegisterError::ForeignGeneration {
        current: self.authority.generation,
      });
    }
    self.authority = authority;
    Ok(())
  }

  /// Answers a new owner's phase-one [`Prepare`]: gates on the installed authority (a prepare under a
  /// generation the holder has not installed is [`ForeignGeneration`](RegisterError::ForeignGeneration);
  /// one from a principal the installed authority does not name as owner is
  /// [`Unauthorized`](RegisterError::Unauthorized)), then raises the fence to the prepare's epoch **before**
  /// replying (so a resumed stale owner under the old epoch can no longer commit — `StaleNeverCommits`),
  /// and reports the whole log so the new owner adopts, per position, the record under the highest epoch
  /// across a phase-one quorum. The reported log is at least as new as anything that ever committed under
  /// the old epoch, because a phase-one quorum and every phase-two commit quorum both intersect at
  /// `f + 1` of `2f + 1`. The promise carries this acceptor's object, so a prepare misrouted to the wrong
  /// object yields a promise that does not [`bind`](LedgerPromise::binds) and is not counted.
  pub fn prepare(&mut self, prepare: &Prepare) -> Result<LedgerPromise, RegisterError> {
    if prepare.generation != self.authority.generation {
      return Err(RegisterError::ForeignGeneration {
        current: self.authority.generation,
      });
    }
    if prepare.owner != self.authority.owner {
      return Err(RegisterError::Unauthorized);
    }
    self.fence.accept(prepare.epoch)?;
    Ok(LedgerPromise {
      holder: self.id,
      object: self.object,
      epoch: prepare.epoch,
      generation: prepare.generation,
      log: self.log.clone(),
    })
  }
}
