//! The fenced register under a network nemesis (§4.8 "Promotion and takeover", "Leases and reads"; goal 81's chaos
//! part, its network half): the register invariants of design models 1 and 4 (`docs/wip/models/FencedRegister.tla`,
//! `FencedLedger.tla`), which TLC proved over the models, checked here as history checks over the real holders
//! ([`Acceptor`]) and the owner's real rules, on every history a seed draws.
//!
//! **The model.** One object and `2f + 1` holders, each a real [`Acceptor`]. Owners are other hosts, each serving one
//! epoch the configuration master hands it, epoch and generation advancing together, one host an epoch. The master
//! installs each new authority on every holder by messages. An owner runs phase one ([`Prepare`] to every holder, a
//! [`Promise`] back, counted when it binds) and adopts the newest record `f + 1` promises report, in the order
//! `promote_over_holders` keeps (`Accepted::newer_than`). It then proposes in order: the adopted record again under its
//! own epoch (BUG-12), then new values, each once the one before it has `f + 1` binding acknowledgements. A record
//! commits once `f + 1` holders have accepted it, whatever reached its owner (the model's `acks`).
//!
//! **The nemesis.** Every message goes over the simulated network:
//! - each is held a latency drawn for it, so messages reorder, and some are lost or delivered twice;
//! - pairs of hosts are cut, one way or both, and healed;
//! - holders are knocked offline (every message to them lost) and come back from their persisted state
//!   (`Acceptor::to_persisted`, `from_persisted`);
//! - owners pause and resume, and a resumed one goes on proposing under its old epoch as if nothing happened;
//! - the master hands out new epochs throughout: a takeover moves the authority to a host under a new generation, and an
//!   owner's restart gives its host the next epoch under the same authority and generation, its old incarnation perhaps
//!   still running.
//!
//! **The history checks**, after every event that can break them:
//! - **Agreement** (TotalOrder): no position commits two values, under any epochs.
//! - **Continuity** (NoLoss for a register): a promoted owner's adopted record is at least as new as every record that
//!   commits under a lower epoch, one partly accepted before the promotion that completes after it included.
//! - **StaleNeverCommits**: a record issued after a higher epoch has its promotion quorum never commits.
//! - **ReadSafety**: at every committed position, every majority's highest-epoch entry is the committed value
//!   (`FencedLedger.tla`'s rule, over the holders' state, an offline holder's included).
//!
//! Three mutations are kept as tests, so the checks are known to bite. Each broke histories (the Mac Studio, 2026-11-18,
//! the first check each broken seed failed):
//! - a promise that raises no fence: 39 of 48 seeds at `f = 1` and 44 of 48 at `f = 2` (StaleNeverCommits, Continuity);
//! - a holder's restart that forgets its fence: 16 and 9 of 192 seeds (StaleNeverCommits, Continuity, Agreement);
//! - an owner that adopts on a minority's promises: 23 and 7 of 48 seeds (Continuity, Agreement).
//!
//! ReadSafety, checked after the others in a step, broke first in none of those, so its own red check is a direct one
//! over real holders' state: a majority whose newest entry at a committed position is another value.
//!
//! The fence carries the load only across an owner's restart. A takeover installs a new generation, and a holder
//! promises only under the generation it has installed, so it refuses the old owner by its authority before its fence
//! is asked. With takeovers alone, neither fence mutation broke a seed (48 seeds each at `f = 1` and `f = 2`).
// Test harness: an unwrap or indexing here is a failed test.
#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};

use crate::register::{
  Accepted, Acceptor, Ack, Authority, HostEpoch, HostId, ObjectId, Prepare, Promise, Record,
};

/// Format: the one object of every run.
const OBJECT: ObjectId = ObjectId([0x51; 16]);
/// Shape: the steps of one run: long enough for several takeovers under every fault.
const STEPS: usize = 1_500;
/// Shape: the seeds each `f` is run over.
const SEEDS: u64 = 48;
/// Shape: the seeds the forgotten-fence restart is run over: four times the others'. At `f = 2` it needs a holder among
/// those that promised an owner's new incarnation to restart before the next takeover, while the old one still runs,
/// and 48 seeds drew none (the Mac Studio, 2026-11-18).
const RESTART_SEEDS: u64 = 4 * SEEDS;
/// Shape: the owner hosts the master hands epochs to, apart from the holders (the models' `Hosts`).
const OWNERS: [HostId; 3] = [HostId(101), HostId(102), HostId(103)];
/// Shape: the epochs one run hands out at most, so each epoch has steps to act in.
const MAX_EPOCHS: u64 = 8;
/// Shape: the longest a message is held, in steps.
const MAX_LATENCY: usize = 12;
/// Shape: the messages the network holds at most; past it the oldest is lost, so the queue is bounded.
const IN_FLIGHT: usize = 256;
/// Shape: the faults' rates, per mille: a message lost, and delivered twice.
const LOSS: u64 = 80;
const DUPLICATE: u64 = 40;
/// Shape: per step, per mille: a pair of hosts cut (one way or both), and a cut healed.
const CUT: u64 = 15;
const HEAL: u64 = 30;
/// Shape: per step, per mille: a holder knocked offline, and an offline one back from its persisted state.
const KNOCK: u64 = 10;
const BACK: u64 = 40;
/// Shape: per step, per mille: an owner paused, and a paused one resumed.
const PAUSE: u64 = 8;
const RESUME: u64 = 25;
/// Shape: per step, per mille: the master hands out the next epoch under a new generation (a takeover), and the
/// current owner's host restarts under the next epoch with its authority unchanged.
const TAKEOVER: u64 = 6;
const OWNER_RESTART: u64 = 4;
/// Shape: per step, per mille: an owner, or the master, sends its outstanding message again.
const RESEND: u64 = 250;
/// Shape: per mille: a new cut that is cut both ways.
const BOTH_WAYS: u64 = 500;
/// Format: the rates' denominator.
const PER_MILLE: u64 = 1_000;
/// Shape: a seed's spread over the generator's state, the golden ratio's 64-bit fraction, so nearby seeds start far
/// apart (the state is made odd, never zero, which xorshift cannot leave).
const SPREAD: u64 = 0x9E37_79B9_7F4A_7C15;

/// A seeded xorshift: the run's only source of choices, so a seed replays exactly.
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 ^= self.0 << 13;
    self.0 ^= self.0 >> 7;
    self.0 ^= self.0 << 17;
    self.0
  }

  fn below(&mut self, bound: usize) -> usize {
    usize::try_from(self.next() % u64::try_from(bound.max(1)).unwrap()).unwrap()
  }

  fn chance(&mut self, per_mille: u64) -> bool {
    self.next() % PER_MILLE < per_mille
  }
}

/// What a message carries.
#[derive(Clone, Debug)]
enum Body {
  Install(Authority),
  Prepare(Prepare),
  Promise(Promise),
  Record(Record),
  Ack(Ack),
}

/// A message in flight, delivered at step `due`.
#[derive(Clone, Debug)]
struct Message {
  from: HostId,
  to: HostId,
  due: usize,
  body: Body,
}

/// A mutation of the holders' rules, kept as a test so the checks are known to bite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mutation {
  /// The holders' rules as they are.
  None,
  /// A holder answers a prepare without raising its fence.
  PromiseWithoutFence,
  /// A holder back from offline has forgotten its fence.
  RestartWithoutFence,
  /// An owner adopts on `f` promises, one short of a quorum.
  PromoteOnMinority,
}

/// One owner: the epoch the master handed a host, and its rounds.
struct Owner {
  prepare: Prepare,
  promised: BTreeSet<HostId>,
  best: Option<Accepted>,
  promoted: bool,
  pending: Option<Record>,
  acked: BTreeSet<HostId>,
  next_sequence: u64,
  paused: bool,
}

/// What the runs counted, so a passing run is known not to be vacuous.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
  takeovers: u64,
  owner_restarts: u64,
  adopted_a_record: u64,
  commits: u64,
  stale_issued: u64,
  stale_accepted: u64,
  completed_after_a_higher_promotion: u64,
  restarts: u64,
  cuts: u64,
  duplicates: u64,
  lost: u64,
}

impl Counts {
  fn add(&mut self, other: Counts) {
    self.takeovers += other.takeovers;
    self.owner_restarts += other.owner_restarts;
    self.adopted_a_record += other.adopted_a_record;
    self.commits += other.commits;
    self.stale_issued += other.stale_issued;
    self.stale_accepted += other.stale_accepted;
    self.completed_after_a_higher_promotion += other.completed_after_a_higher_promotion;
    self.restarts += other.restarts;
    self.cuts += other.cuts;
    self.duplicates += other.duplicates;
    self.lost += other.lost;
  }
}

/// The oracle: what was issued, accepted, promised, adopted and committed, and the checks over it.
#[derive(Default)]
struct History {
  /// Each record issued, by identity, and whether a higher epoch had its promotion quorum when it was issued.
  issued: BTreeMap<[u8; 32], (Record, bool)>,
  /// The holders that accepted each record.
  accepted_by: BTreeMap<[u8; 32], BTreeSet<HostId>>,
  /// The records committed, by identity.
  committed: BTreeMap<[u8; 32], Record>,
  /// The value committed at each position.
  committed_at: BTreeMap<u64, Vec<u8>>,
  /// The holders that raised their fence to each epoch (phase one's acks).
  promised: BTreeMap<u64, BTreeSet<HostId>>,
  /// The record each promoted epoch adopted, `None` when its quorum reported nothing.
  adopted: BTreeMap<u64, Option<Accepted>>,
  /// The first violation of each check, with its step.
  violations: Vec<String>,
  counts: Counts,
}

/// Whether `held` is at least as new as `record` in phase one's order: position, then epoch.
fn at_least_as_new(held: Option<&Accepted>, record: &Record) -> bool {
  held.is_some_and(|held| (held.sequence, held.epoch) >= (record.sequence, record.epoch))
}

impl History {
  fn quorum_promised(&self, epoch: u64, quorum: usize) -> bool {
    self
      .promised
      .get(&epoch)
      .is_some_and(|holders| holders.len() >= quorum)
  }

  /// Whether some epoch above `epoch` has its promotion quorum.
  fn higher_promoted(&self, epoch: u64, quorum: usize) -> bool {
    self
      .promised
      .keys()
      .any(|higher| *higher > epoch && self.quorum_promised(*higher, quorum))
  }

  fn violate(&mut self, step: usize, said: String) {
    self.violations.push(format!("step {step}: {said}"));
  }

  /// An owner issued `record`: it is stale when a higher epoch already has its promotion quorum.
  fn issue(&mut self, record: &Record, quorum: usize) {
    let stale = self.higher_promoted(record.epoch.0, quorum);
    if stale {
      self.counts.stale_issued += 1;
    }
    self
      .issued
      .entry(record.identity())
      .or_insert_with(|| (record.clone(), stale));
  }

  /// A holder accepted `record`: a commit once `quorum` holders have.
  fn accept(&mut self, step: usize, holder: HostId, record: &Record, quorum: usize) {
    let identity = record.identity();
    let stale = self.issued.get(&identity).is_some_and(|(_, stale)| *stale);
    let holders = self.accepted_by.entry(identity).or_default();
    let first = holders.is_empty();
    holders.insert(holder);
    let reached = holders.len() >= quorum;
    if first && stale {
      self.counts.stale_accepted += 1;
    }
    if reached && !self.committed.contains_key(&identity) {
      self.commit(step, record, stale, quorum);
    }
  }

  /// `record` committed: Agreement, StaleNeverCommits and Continuity.
  fn commit(&mut self, step: usize, record: &Record, stale: bool, quorum: usize) {
    self.counts.commits += 1;
    self.committed.insert(record.identity(), record.clone());
    if self.higher_promoted(record.epoch.0, quorum) {
      self.counts.completed_after_a_higher_promotion += 1;
    }
    if stale {
      self.violate(
        step,
        format!(
          "StaleNeverCommits: position {} committed under epoch {} after a higher epoch's promotion quorum",
          record.sequence, record.epoch.0
        ),
      );
    }
    match self.committed_at.get(&record.sequence) {
      Some(value) if *value != record.value => {
        let said = format!(
          "Agreement: position {} committed a second value under epoch {}",
          record.sequence, record.epoch.0
        );
        self.violate(step, said);
      }
      Some(_) => {}
      None => {
        self
          .committed_at
          .insert(record.sequence, record.value.clone());
      }
    }
    let short: Vec<u64> = self
      .adopted
      .iter()
      .filter(|(epoch, adopted)| {
        **epoch > record.epoch.0 && !at_least_as_new(adopted.as_ref(), record)
      })
      .map(|(epoch, _)| *epoch)
      .collect();
    for epoch in short {
      self.violate(
        step,
        format!(
          "Continuity: epoch {epoch} adopted less than position {} committed under epoch {}",
          record.sequence, record.epoch.0
        ),
      );
    }
  }

  /// Epoch `epoch` adopted `adopted`: Continuity against every record committed under a lower epoch.
  fn adopt(&mut self, step: usize, epoch: u64, adopted: Option<Accepted>) {
    if adopted.is_some() {
      self.counts.adopted_a_record += 1;
    }
    let short: Vec<(u64, u64)> = self
      .committed
      .values()
      .filter(|record| record.epoch.0 < epoch && !at_least_as_new(adopted.as_ref(), record))
      .map(|record| (record.sequence, record.epoch.0))
      .collect();
    for (sequence, committed_epoch) in short {
      self.violate(
        step,
        format!(
          "Continuity: epoch {epoch} adopted less than position {sequence} committed under epoch {committed_epoch}"
        ),
      );
    }
    self.adopted.insert(epoch, adopted);
  }

  /// ReadSafety at `position`: every `quorum` of `holders` reads the committed value as its highest-epoch entry.
  fn read_safety(
    &mut self,
    step: usize,
    position: u64,
    holders: &BTreeMap<HostId, Holder>,
    quorum: usize,
  ) {
    let Some(value) = self.committed_at.get(&position).cloned() else {
      return;
    };
    let ids: Vec<HostId> = holders.keys().copied().collect();
    for majority in subsets(&ids, quorum) {
      let highest = majority
        .iter()
        .filter_map(|id| holders[id].acceptor.position(OBJECT, position))
        .max_by_key(|(epoch, _)| *epoch);
      if highest.is_none_or(|(_, read)| read != value.as_slice()) {
        let said = format!(
          "ReadSafety: a majority {majority:?} reads position {position} other than its committed value"
        );
        self.violate(step, said);
        return;
      }
    }
  }
}

/// Every subset of `ids` of `size` members.
fn subsets(ids: &[HostId], size: usize) -> Vec<Vec<HostId>> {
  if size == 0 {
    return vec![Vec::new()];
  }
  let Some((first, rest)) = ids.split_first() else {
    return Vec::new();
  };
  let mut with: Vec<Vec<HostId>> = subsets(rest, size - 1)
    .into_iter()
    .map(|mut subset| {
      subset.insert(0, *first);
      subset
    })
    .collect();
  with.extend(subsets(rest, size));
  with
}

/// One holder: its acceptor, and whether it is online.
struct Holder {
  acceptor: Acceptor,
  online: bool,
}

/// One run's world.
struct World {
  rng: Rng,
  mutation: Mutation,
  quorum: usize,
  step: usize,
  holders: BTreeMap<HostId, Holder>,
  owners: BTreeMap<u64, Owner>,
  epoch: u64,
  generation: u64,
  authority_owner: HostId,
  in_flight: Vec<Message>,
  cuts: BTreeSet<(HostId, HostId)>,
  values: u64,
  history: History,
}

impl World {
  fn new(seed: u64, f: usize, mutation: Mutation) -> World {
    let nobody = Authority {
      generation: 0,
      owner: HostId(0),
    };
    let holders = (1..=2 * f + 1)
      .map(|index| {
        let id = HostId(u64::try_from(index).unwrap());
        let holder = Holder {
          acceptor: Acceptor::new(id, nobody),
          online: true,
        };
        (id, holder)
      })
      .collect();
    World {
      rng: Rng(seed.wrapping_mul(SPREAD) | 1),
      mutation,
      quorum: f + 1,
      step: 0,
      holders,
      owners: BTreeMap::new(),
      epoch: 0,
      generation: 0,
      authority_owner: HostId(0),
      in_flight: Vec::new(),
      cuts: BTreeSet::new(),
      values: 0,
      history: History::default(),
    }
  }

  fn hosts(&self) -> Vec<HostId> {
    self.holders.keys().copied().chain(OWNERS).collect()
  }

  /// Sends `body` from `from` to `to`: lost, or held a drawn latency, and perhaps sent twice.
  fn send(&mut self, from: HostId, to: HostId, body: Body) {
    if self.rng.chance(LOSS) {
      self.history.counts.lost += 1;
      return;
    }
    let copies = if self.rng.chance(DUPLICATE) { 2 } else { 1 };
    if copies == 2 {
      self.history.counts.duplicates += 1;
    }
    for _ in 0..copies {
      let due = self.step + 1 + self.rng.below(MAX_LATENCY);
      self.in_flight.push(Message {
        from,
        to,
        due,
        body: body.clone(),
      });
    }
    while self.in_flight.len() > IN_FLIGHT {
      self.in_flight.remove(0);
      self.history.counts.lost += 1;
    }
  }

  fn send_to_holders(&mut self, from: HostId, body: &Body) {
    let holders: Vec<HostId> = self.holders.keys().copied().collect();
    for holder in holders {
      self.send(from, holder, body.clone());
    }
  }

  /// The master hands the next epoch to a host under a new generation and installs its authority on every holder by
  /// messages.
  fn takeover(&mut self) {
    self.generation += 1;
    self.history.counts.takeovers += 1;
    let host = OWNERS[self.rng.below(OWNERS.len())];
    self.authority_owner = host;
    self.begin_owner(host);
  }

  /// The current owner's host restarts under the next epoch with its authority unchanged (the same generation and owner,
  /// so nothing is installed): its old incarnation may still run, and only the fence tells the two apart.
  fn owner_restart(&mut self) {
    self.history.counts.owner_restarts += 1;
    let host = self.authority_owner;
    self.begin_owner(host);
  }

  /// A new owner of the next epoch on `host`, under the current generation: it installs the authority and runs phase one.
  fn begin_owner(&mut self, host: HostId) {
    self.epoch += 1;
    let prepare = Prepare {
      owner: host,
      object: OBJECT,
      epoch: HostEpoch(self.epoch),
      generation: self.generation,
    };
    self.owners.insert(
      self.epoch,
      Owner {
        prepare,
        promised: BTreeSet::new(),
        best: None,
        promoted: false,
        pending: None,
        acked: BTreeSet::new(),
        next_sequence: 0,
        paused: false,
      },
    );
    self.install_latest();
    self.send_to_holders(host, &Body::Prepare(prepare));
  }

  /// The master sends the newest authority to every holder (the configuration's distribution, retried).
  fn install_latest(&mut self) {
    if self.generation > 0 {
      let authority = Authority {
        generation: self.generation,
        owner: self.authority_owner,
      };
      self.send_to_holders(HostId(0), &Body::Install(authority));
    }
  }

  /// One step of the nemesis: faults drawn, then every owner's turn, then every message due delivered.
  fn step(&mut self) {
    self.faults();
    let epochs: Vec<u64> = self.owners.keys().copied().collect();
    for epoch in epochs {
      self.owner_turn(epoch);
    }
    self.deliver_due();
    self.step += 1;
  }

  fn faults(&mut self) {
    let more = self.epoch < MAX_EPOCHS;
    if self.epoch == 0 || (more && self.rng.chance(TAKEOVER)) {
      self.takeover();
    } else if more && self.rng.chance(OWNER_RESTART) {
      self.owner_restart();
    } else if self.rng.chance(RESEND) {
      self.install_latest();
    }
    self.cut_or_heal();
    self.knock_or_return();
    self.pause_or_resume();
  }

  fn cut_or_heal(&mut self) {
    if self.rng.chance(CUT) {
      let hosts = self.hosts();
      let (a, b) = (
        hosts[self.rng.below(hosts.len())],
        hosts[self.rng.below(hosts.len())],
      );
      if a != b {
        self.history.counts.cuts += 1;
        self.cuts.insert((a, b));
        if self.rng.chance(BOTH_WAYS) {
          self.cuts.insert((b, a));
        }
      }
    }
    if self.rng.chance(HEAL) && !self.cuts.is_empty() {
      let index = self.rng.below(self.cuts.len());
      let cut = *self.cuts.iter().nth(index).unwrap();
      self.cuts.remove(&cut);
    }
  }

  fn knock_or_return(&mut self) {
    let ids: Vec<HostId> = self.holders.keys().copied().collect();
    let id = ids[self.rng.below(ids.len())];
    let online = self.holders[&id].online;
    if online && self.rng.chance(KNOCK) {
      self.holders.get_mut(&id).unwrap().online = false;
    } else if !online && self.rng.chance(BACK) {
      self.history.counts.restarts += 1;
      let mut persisted = self.holders[&id].acceptor.to_persisted();
      if self.mutation == Mutation::RestartWithoutFence {
        persisted.promised = 0;
      }
      let holder = self.holders.get_mut(&id).unwrap();
      holder.acceptor = Acceptor::from_persisted(id, persisted);
      holder.online = true;
    }
  }

  fn pause_or_resume(&mut self) {
    if self.owners.is_empty() {
      return;
    }
    let epochs: Vec<u64> = self.owners.keys().copied().collect();
    let epoch = epochs[self.rng.below(epochs.len())];
    let paused = self.owners[&epoch].paused;
    if !paused && self.rng.chance(PAUSE) {
      self.owners.get_mut(&epoch).unwrap().paused = true;
    } else if paused && self.rng.chance(RESUME) {
      self.owners.get_mut(&epoch).unwrap().paused = false;
    }
  }

  /// An owner's turn: phase one until promoted, then its next record, each sent again at the resend rate.
  fn owner_turn(&mut self, epoch: u64) {
    let resend = self.rng.chance(RESEND);
    let owner = &self.owners[&epoch];
    if owner.paused {
      return;
    }
    let host = owner.prepare.owner;
    if !owner.promoted {
      if resend {
        let prepare = owner.prepare;
        self.send_to_holders(host, &Body::Prepare(prepare));
      }
      return;
    }
    let fresh = owner.pending.is_none();
    if fresh {
      self.issue_next(epoch);
    }
    if fresh || resend {
      if let Some(record) = self.owners[&epoch].pending.clone() {
        self.send_to_holders(host, &Body::Record(record));
      }
    }
  }

  /// The owner of `epoch` issues its next record: a new value at its next position.
  fn issue_next(&mut self, epoch: u64) {
    self.values += 1;
    let owner = &self.owners[&epoch];
    let record = Record {
      owner: owner.prepare.owner,
      object: OBJECT,
      sequence: owner.next_sequence,
      epoch: owner.prepare.epoch,
      generation: owner.prepare.generation,
      value: format!("e{epoch}-n{}", self.values).into_bytes(),
    };
    self.history.issue(&record, self.quorum);
    let owner = self.owners.get_mut(&epoch).unwrap();
    owner.pending = Some(record);
    owner.acked.clear();
  }

  fn deliver_due(&mut self) {
    let (due, later): (Vec<Message>, Vec<Message>) = std::mem::take(&mut self.in_flight)
      .into_iter()
      .partition(|message| message.due <= self.step);
    self.in_flight = later;
    let mut due = due;
    // Messages due together arrive in an order the seed chooses.
    for index in (1..due.len()).rev() {
      let other = self.rng.below(index + 1);
      due.swap(index, other);
    }
    for message in due {
      if self.cuts.contains(&(message.from, message.to)) {
        self.history.counts.lost += 1;
        continue;
      }
      self.deliver(message);
    }
  }

  fn deliver(&mut self, message: Message) {
    match message.body {
      Body::Install(authority) => self.on_install(message.to, authority),
      Body::Prepare(prepare) => self.on_prepare(message.to, &prepare),
      Body::Record(record) => self.on_record(message.to, &record),
      Body::Promise(promise) => self.on_promise(message.to, &promise),
      Body::Ack(ack) => self.on_ack(message.to, &ack),
    }
  }

  /// The online holder `to`, if `to` is one.
  fn online_holder(&mut self, to: HostId) -> Option<&mut Acceptor> {
    self
      .holders
      .get_mut(&to)
      .filter(|holder| holder.online)
      .map(|holder| &mut holder.acceptor)
  }

  fn on_install(&mut self, to: HostId, authority: Authority) {
    if let Some(acceptor) = self.online_holder(to) {
      let _refused_older = acceptor.install_authority(authority);
    }
  }

  fn on_prepare(&mut self, to: HostId, prepare: &Prepare) {
    let mutation = self.mutation;
    let Some(acceptor) = self.online_holder(to) else {
      return;
    };
    let answered = if mutation == Mutation::PromiseWithoutFence {
      promise_without_fence(to, acceptor, prepare)
    } else {
      acceptor.prepare(prepare).ok()
    };
    if let Some(promise) = answered {
      self
        .history
        .promised
        .entry(prepare.epoch.0)
        .or_default()
        .insert(to);
      self.send(to, prepare.owner, Body::Promise(promise));
    }
  }

  fn on_record(&mut self, to: HostId, record: &Record) {
    let Some(acceptor) = self.online_holder(to) else {
      return;
    };
    let Ok(ack) = acceptor.accept(record) else {
      return;
    };
    let (step, quorum) = (self.step, self.quorum);
    self.history.accept(step, to, record, quorum);
    self
      .history
      .read_safety(step, record.sequence, &self.holders, quorum);
    self.send(to, record.owner, Body::Ack(ack));
  }

  /// A promise reaches every owner of its host that is not paused; the one it binds counts it.
  fn on_promise(&mut self, to: HostId, promise: &Promise) {
    let promotion = if self.mutation == Mutation::PromoteOnMinority {
      self.quorum - 1
    } else {
      self.quorum
    };
    let mut promoted = None;
    for (epoch, owner) in &mut self.owners {
      if owner.paused
        || owner.promoted
        || owner.prepare.owner != to
        || !promise.binds(&owner.prepare)
      {
        continue;
      }
      owner.promised.insert(promise.holder);
      if let Some(reported) = &promise.highest
        && owner
          .best
          .as_ref()
          .is_none_or(|best| reported.newer_than(best))
      {
        owner.best = Some(reported.clone());
      }
      if owner.promised.len() >= promotion {
        owner.promoted = true;
        promoted = Some((*epoch, owner.best.clone()));
      }
    }
    if let Some((epoch, adopted)) = promoted {
      let step = self.step;
      self.history.adopt(step, epoch, adopted.clone());
      self.begin_after_adoption(epoch, adopted);
    }
  }

  /// A promoted owner proposes the adopted record again under its own epoch first (BUG-12), then goes on past it.
  fn begin_after_adoption(&mut self, epoch: u64, adopted: Option<Accepted>) {
    let Some(adopted) = adopted else {
      return;
    };
    let owner = &self.owners[&epoch];
    let record = Record {
      owner: owner.prepare.owner,
      object: OBJECT,
      sequence: adopted.sequence,
      epoch: owner.prepare.epoch,
      generation: owner.prepare.generation,
      value: adopted.value,
    };
    self.history.issue(&record, self.quorum);
    let owner = self.owners.get_mut(&epoch).unwrap();
    owner.next_sequence = record.sequence;
    owner.pending = Some(record);
    owner.acked.clear();
  }

  /// An acknowledgement reaches every owner of its host that is not paused; the one whose record it binds counts it.
  fn on_ack(&mut self, to: HostId, ack: &Ack) {
    let quorum = self.quorum;
    for owner in self.owners.values_mut() {
      if owner.paused || owner.prepare.owner != to {
        continue;
      }
      let Some(pending) = &owner.pending else {
        continue;
      };
      if !ack.binds(pending) {
        continue;
      }
      owner.acked.insert(ack.holder);
      if owner.acked.len() >= quorum {
        owner.next_sequence = pending.sequence + 1;
        owner.pending = None;
        owner.acked.clear();
      }
    }
  }
}

/// The mutation of [`Acceptor::prepare`] that raises no fence: holder `holder`'s same checks of generation, owner and
/// epoch, the same report of its highest record, and its fence left where it was.
fn promise_without_fence(
  holder: HostId,
  acceptor: &Acceptor,
  prepare: &Prepare,
) -> Option<Promise> {
  let authority = acceptor.authority();
  if prepare.generation != authority.generation
    || prepare.owner != authority.owner
    || prepare.epoch < acceptor.fence()
  {
    return None;
  }
  Some(Promise {
    holder,
    object: prepare.object,
    epoch: prepare.epoch,
    generation: authority.generation,
    highest: acceptor.highest_accepted(prepare.object),
  })
}

/// Runs `seeds` histories at `f` under `mutation`: the violations each broken seed found first, and what all counted.
fn run(f: usize, seeds: u64, mutation: Mutation) -> (Vec<(u64, String)>, Counts) {
  let mut broken = Vec::new();
  let mut counts = Counts::default();
  for seed in 0..seeds {
    let mut world = World::new(seed, f, mutation);
    while world.step < STEPS && world.history.violations.is_empty() {
      world.step();
    }
    counts.add(world.history.counts);
    if let Some(first) = world.history.violations.first() {
      broken.push((seed, first.clone()));
    }
  }
  (broken, counts)
}

/// Prints, for a mutation's broken seeds, how many first broke each check.
fn say_broken(mutation: Mutation, f: usize, seeds: u64, broken: &[(u64, String)]) {
  let tally: BTreeMap<&str, usize> = broken.iter().fold(BTreeMap::new(), |mut tally, (_, said)| {
    let check = ["Agreement", "Continuity", "StaleNeverCommits", "ReadSafety"]
      .into_iter()
      .find(|check| said.contains(check))
      .unwrap_or("other");
    *tally.entry(check).or_default() += 1;
    tally
  });
  println!(
    "register nemesis, {mutation:?} at f = {f}: {} of {seeds} seeds broke, first breaking {tally:?}",
    broken.len()
  );
}

/// Goal 81's chaos part, its network half (§4.8; design models 1 and 4). Do: run the nemesis over [`SEEDS`] seeds at
/// `f = 1` and `f = 2` (three and five holders): latency, loss, duplication, reordering, one-way and two-way cuts,
/// holders knocked offline and back from their persisted state, owners paused and resumed, new epochs throughout.
/// Expect: no history breaks Agreement, Continuity, StaleNeverCommits or ReadSafety. So the run is not vacuous:
/// takeovers, adoptions of a record, commits, stale records issued and accepted somewhere, records completing after a
/// higher epoch's promotion, restarts, cuts, duplicates and losses are all counted.
#[test]
fn the_fenced_register_holds_its_invariants_under_the_network_nemesis() {
  for f in [1, 2] {
    let (broken, counts) = run(f, SEEDS, Mutation::None);
    println!("register nemesis at f = {f}: {counts:?}");
    assert!(broken.is_empty(), "f = {f}: {broken:#?}");
    for (what, count) in [
      ("takeovers", counts.takeovers),
      ("owner restarts", counts.owner_restarts),
      ("adoptions of a record", counts.adopted_a_record),
      ("commits", counts.commits),
      ("stale records issued", counts.stale_issued),
      ("stale records accepted somewhere", counts.stale_accepted),
      (
        "records completing after a higher promotion",
        counts.completed_after_a_higher_promotion,
      ),
      ("restarts", counts.restarts),
      ("cuts", counts.cuts),
      ("duplicates", counts.duplicates),
      ("losses", counts.lost),
    ] {
      assert!(count > 0, "f = {f}: no {what} in {SEEDS} seeds");
    }
  }
}

/// The checks' teeth, kept as a test: a holder that answers a prepare without raising its fence. Do: run the nemesis
/// with that mutation at `f = 1` and `f = 2`. Expect: some seed breaks a check, since an owner's old incarnation, which
/// shares its authority and generation, then commits past the new incarnation's promotion.
#[test]
fn a_promise_that_raises_no_fence_breaks_the_register_under_the_nemesis() {
  for f in [1, 2] {
    let (broken, _) = run(f, SEEDS, Mutation::PromiseWithoutFence);
    say_broken(Mutation::PromiseWithoutFence, f, SEEDS, &broken);
    assert!(
      !broken.is_empty(),
      "f = {f}: no seed broke a check with promises that raise no fence"
    );
  }
}

/// The checks' teeth, kept as a test: a holder back from offline that has forgotten its fence. Do: run the nemesis
/// with that mutation at `f = 1` and `f = 2`. Expect: some seed breaks a check, since an owner's old incarnation's
/// records are then accepted again by a holder that had promised its new one.
#[test]
fn a_restart_that_forgets_the_fence_breaks_the_register_under_the_nemesis() {
  for f in [1, 2] {
    let (broken, _) = run(f, RESTART_SEEDS, Mutation::RestartWithoutFence);
    say_broken(Mutation::RestartWithoutFence, f, RESTART_SEEDS, &broken);
    assert!(
      !broken.is_empty(),
      "f = {f}: no seed broke a check with restarts that forget the fence"
    );
  }
}

/// The checks' teeth, kept as a test: an owner that adopts on `f` promises, one short of a quorum. Do: run the nemesis
/// with that mutation at `f = 1` and `f = 2`. Expect: some seed breaks a check, since a minority's report can miss a
/// record that committed, so the adopted record is older than it (Continuity) and the owner writes over it.
#[test]
fn an_owner_that_promotes_on_a_minority_breaks_the_register_under_the_nemesis() {
  for f in [1, 2] {
    let (broken, _) = run(f, SEEDS, Mutation::PromoteOnMinority);
    say_broken(Mutation::PromoteOnMinority, f, SEEDS, &broken);
    assert!(
      !broken.is_empty(),
      "f = {f}: no seed broke a check with owners that promote on a minority"
    );
  }
}

/// ReadSafety's check bites, its own red check: a position committed `v` at epoch 1 on two of three holders, and the third
/// holding `w` at epoch 2, as a holder would if an owner of a higher epoch wrote the position without having adopted
/// `v`. Do: ask the check, over the real holders' state. Expect: a ReadSafety violation, since a majority with the third
/// holder reads `w`, its highest-epoch entry. The nemesis's mutations break other checks first, so this is where
/// ReadSafety's own rule is shown to tell.
#[test]
fn the_read_safety_check_finds_a_majority_whose_newest_entry_is_another_value() {
  let first = Authority {
    generation: 1,
    owner: OWNERS[0],
  };
  let second = Authority {
    generation: 2,
    owner: OWNERS[1],
  };
  let at_zero = |authority: Authority, epoch: u64, value: &[u8]| Record {
    owner: authority.owner,
    object: OBJECT,
    sequence: 0,
    epoch: HostEpoch(epoch),
    generation: authority.generation,
    value: value.to_vec(),
  };
  let mut holders = BTreeMap::new();
  for id in [HostId(1), HostId(2)] {
    let mut acceptor = Acceptor::new(id, first);
    acceptor.accept(&at_zero(first, 1, b"v")).unwrap();
    holders.insert(
      id,
      Holder {
        acceptor,
        online: true,
      },
    );
  }
  let mut third = Acceptor::new(HostId(3), second);
  third.accept(&at_zero(second, 2, b"w")).unwrap();
  holders.insert(
    HostId(3),
    Holder {
      acceptor: third,
      online: true,
    },
  );
  let mut history = History::default();
  history.committed_at.insert(0, b"v".to_vec());
  history.read_safety(0, 0, &holders, 2);
  assert!(
    history
      .violations
      .iter()
      .any(|said| said.contains("ReadSafety")),
    "the check names the majority that reads w: {:?}",
    history.violations
  );
}
