//! Live shipping's log under a nemesis (A-200; §4.10; AC-8.3, AC-8.9): the owner ships publications to its `2f`
//! holders over a network that loses, duplicates and reorders messages and partitions pairs of hosts; holders crash
//! for good; owners are lost and taken over by phase one; a superseded owner keeps shipping as if nothing happened.
//! The register invariants are history checks, after every step:
//!
//! - **Agreement**: no position is ever durable with two values, whichever owner made it durable.
//! - **NoLoss**: every takeover adopts a head whose state holds every durable entry, at its position, unchanged.
//! - **StaleNeverCommits**: whatever a superseded owner still makes durable lies inside the head that superseded it.
//! - **Convergence**: once the network heals, every live holder holds exactly the owner's state, and every durable
//!   write is in it.
//!
//! The payload of each entry is one written value; a replica's state is the values it applied, one per position, so
//! its length is its head's position plus one. An install carries the whole vector.

// Test harness: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::indexing_slicing,
  clippy::too_many_lines,
  clippy::panic
)]

use std::collections::{BTreeMap, BTreeSet};

use slates_db::live::{
  Admitted, Entry, Head, Install, LiveError, Next, Replica, Shipper, adopt, decode_reply,
  encode_reply,
};
use slates_db::register::{HostEpoch, HostId, ObjectId, Quorum};

/// Format: the one volume of every run.
const OBJECT: ObjectId = ObjectId([7; 16]);
/// Shape: the steps of one run: long enough for several takeovers, catch-ups and installs.
const STEPS: usize = 3_000;
/// Shape: the seeds each `f` is run over.
const SEEDS: u64 = 200;
/// Shape: the cap a holder applies to a decoded payload or state (the model's are tiny).
const CAP: usize = 1 << 20;

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
    self.next() % 1000 < per_mille
  }
}

#[derive(Clone, Debug)]
enum Body {
  Entry(Vec<u8>),
  Install(Vec<u8>),
  /// A holder's reply, for the owner of `shipper` (an index into the run's owners).
  Reply(Vec<u8>),
}

#[derive(Clone, Debug)]
struct Message {
  from: HostId,
  to: HostId,
  /// The owner instance a reply returns to, or that sent an entry or install.
  shipper: usize,
  body: Body,
}

/// One owner instance: its shipper and its state. Superseded ones stay and keep shipping.
struct Owner {
  host: HostId,
  shipper: Shipper,
  state: Vec<u64>,
  /// Set once a takeover superseded it: the adopted head's position and the adopted state.
  superseded: Option<(Option<Head>, Vec<u64>)>,
  /// Set once a holder's reply told it a later fence: it stops.
  fenced: bool,
}

struct Holder {
  /// Its copy of the log: `None` until it first accepts a message, as the daemon keeps a candidate's copy only once one
  /// arrives (`slates_server::live_ship::held_entry`).
  replica: Option<Replica>,
  /// The owner the configuration names to it, as far as it has learned (`learn`): the only sender whose message makes
  /// it a copy (`slates_server::live_ship::admitted`). A takeover's fetch does not change it.
  configured: HostId,
  state: Vec<u64>,
  alive: bool,
}

/// What phase one does with a candidate that holds no copy of the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EmptyReport {
  /// The design (A-200): it makes an empty copy, raises its fence to the takeover's epoch and reports nothing held; the
  /// report counts toward `f + 1`.
  FenceAndCount,
  /// A mutation, the daemon before 2026-11-05 for a remote candidate: it reports nothing held and keeps no fence.
  CountWithoutFence,
}

fn encode_state(state: &[u64]) -> Vec<u8> {
  state.iter().flat_map(|value| value.to_le_bytes()).collect()
}

fn decode_state(bytes: &[u8]) -> Vec<u64> {
  bytes
    .as_chunks::<8>()
    .0
    .iter()
    .map(|chunk| u64::from_le_bytes(*chunk))
    .collect()
}

/// A holder's copy for a message from `from`: its own, or a new one when it has none and the configuration it knows
/// (`configured`) names `from` the owner; `None` (the message refused unanswered) for a stranger.
fn copy_for(
  replica: &mut Option<Replica>,
  configured: HostId,
  from: HostId,
) -> Option<&mut Replica> {
  if replica.is_none() && from != configured {
    return None;
  }
  Some(replica.get_or_insert_with(|| Replica::new(configured, OBJECT)))
}

/// A holder learns the configuration's owner and epoch: its copy, if it has one, is fenced and handed to `owner`.
fn learn_owner(holder: &mut Holder, owner: HostId, epoch: HostEpoch) {
  holder.configured = owner;
  if let Some(replica) = &mut holder.replica
    && replica.prepare(epoch).is_ok()
  {
    replica.reassign(owner);
  }
}

struct Run {
  f: u64,
  rng: Rng,
  hosts: Vec<HostId>,
  holders: BTreeMap<HostId, Holder>,
  owners: Vec<Owner>,
  /// The current owner instance.
  current: usize,
  network: Vec<Message>,
  cut: BTreeSet<(HostId, HostId)>,
  /// Agreement: every durable position's value.
  durable: BTreeMap<u64, u64>,
  next_value: u64,
  epoch: u64,
  lost: u64,
  takeovers: u64,
  installs: u64,
  stale_refusals: u64,
  /// Phase one's reports counted from candidates that held no copy.
  empty_reports: u64,
  rule: EmptyReport,
  /// Whether the history is printed as it happens ([`Run::traced`]): what each step did that bears on the invariants.
  traced: bool,
}

impl Run {
  fn new(f: u64, seed: u64, rule: EmptyReport) -> Run {
    let hosts: Vec<HostId> = (1..=2 * f + 1).map(HostId).collect();
    let owner = hosts[0];
    let holders = hosts
      .iter()
      .filter(|host| **host != owner)
      .map(|host| {
        (
          *host,
          Holder {
            replica: None,
            configured: owner,
            state: Vec::new(),
            alive: true,
          },
        )
      })
      .collect();
    let shipper = Shipper::new(
      (owner, OBJECT, HostEpoch(1)),
      Quorum {
        f: u32::try_from(f).unwrap(),
      },
      &hosts,
      None,
      (8, 16),
    );
    Run {
      f,
      rng: Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1),
      hosts,
      holders,
      owners: vec![Owner {
        host: owner,
        shipper,
        state: Vec::new(),
        superseded: None,
        fenced: false,
      }],
      current: 0,
      network: Vec::new(),
      cut: BTreeSet::new(),
      durable: BTreeMap::new(),
      next_value: 1,
      epoch: 1,
      lost: 0,
      takeovers: 0,
      installs: 0,
      stale_refusals: 0,
      empty_reports: 0,
      rule,
      traced: false,
    }
  }

  /// This run with its history kept, for a failing seed's record.
  fn traced(mut self) -> Run {
    self.traced = true;
    self
  }

  fn note(&self, event: impl FnOnce() -> String) {
    if self.traced {
      eprintln!("  {}", event());
    }
  }

  fn send(&mut self, from: HostId, to: HostId, shipper: usize, body: Body) {
    self.network.push(Message {
      from,
      to,
      shipper,
      body,
    });
  }

  fn send_next(&mut self, index: usize, holder: HostId, next: Next) {
    let from = self.owners[index].host;
    match next {
      Next::Nothing => {}
      Next::Fenced(_) => self.owners[index].fenced = true,
      Next::Entries(entries) => {
        for entry in entries {
          self.send(from, holder, index, Body::Entry(entry.encode()));
        }
      }
      Next::Install => {
        let owner = &self.owners[index];
        if let Some(install) = owner.shipper.install(encode_state(&owner.state)) {
          self.send(from, holder, index, Body::Install(install.encode()));
        }
      }
    }
  }

  /// An owner writes the next value: appends it and ships the entry to every holder.
  fn write(&mut self, index: usize) {
    let value = self.next_value;
    let owner = &mut self.owners[index];
    if owner.fenced {
      return;
    }
    match owner.shipper.append(value.to_le_bytes().to_vec()) {
      Ok(entry) => {
        self.next_value += 1;
        owner.state.push(value);
        assert_eq!(
          u64::try_from(owner.state.len()).unwrap(),
          entry.position + 1,
          "an owner's state is one value per position"
        );
        let from = owner.host;
        let holders = owner.shipper.holders().to_vec();
        let epoch = owner.shipper.epoch().0;
        self.note(|| {
          format!(
            "owner {} (host {}, epoch {epoch}) appends position {} = {value}",
            index, from.0, entry.position
          )
        });
        for holder in holders {
          self.send(from, holder, index, Body::Entry(entry.encode()));
        }
        self.note_durable(index);
      }
      Err(LiveError::NoCredit { .. }) => {}
      Err(refusal) => panic!("an append was refused {refusal:?}"),
    }
  }

  /// Re-ships to every holder what its last known head lacks (the owner's period).
  fn tick(&mut self, index: usize) {
    if self.owners[index].fenced {
      return;
    }
    let holders = self.owners[index].shipper.holders().to_vec();
    for holder in holders {
      let next = self.owners[index].shipper.pending(holder);
      self.send_next(index, holder, next);
    }
  }

  fn deliver(&mut self, at: usize) {
    let message = self.network.swap_remove(at);
    if self.cut.contains(&(message.from, message.to))
      || self.cut.contains(&(message.to, message.from))
    {
      return;
    }
    match message.body {
      Body::Entry(bytes) => {
        let entry = Entry::decode(&bytes, CAP).unwrap();
        let Some(holder) = self.holders.get_mut(&message.to) else {
          return;
        };
        if !holder.alive {
          return;
        }
        let made = holder.replica.is_none();
        let Some(replica) = copy_for(&mut holder.replica, holder.configured, message.from) else {
          return;
        };
        let reply = match replica.check(&entry) {
          Ok(Admitted::Apply(head)) => {
            holder.state.push(u64::from_le_bytes(
              entry.payload.as_slice().try_into().unwrap(),
            ));
            replica.commit(entry.epoch, head);
            let (to, from, position, epoch) =
              (message.to.0, message.from.0, entry.position, entry.epoch.0);
            self.note(|| {
              format!(
                "host {to} accepts position {position} from host {from} (epoch {epoch}){}",
                if made { ", making its copy" } else { "" }
              )
            });
            Ok(Some(head))
          }
          Ok(Admitted::Held) => Ok(replica.head()),
          Err(refusal) => Err(refusal),
        };
        if matches!(reply, Err(LiveError::StaleEpoch { .. })) {
          self.stale_refusals += 1;
        }
        self.send(
          message.to,
          message.from,
          message.shipper,
          Body::Reply(encode_reply(&reply)),
        );
      }
      Body::Install(bytes) => {
        let install = Install::decode(&bytes, CAP).unwrap();
        let Some(holder) = self.holders.get_mut(&message.to) else {
          return;
        };
        if !holder.alive {
          return;
        }
        let Some(replica) = copy_for(&mut holder.replica, holder.configured, message.from) else {
          return;
        };
        let reply = match replica.check_install(&install) {
          Ok(Admitted::Apply(head)) => {
            holder.state = decode_state(&install.state);
            replica.commit(install.epoch, head);
            self.installs += 1;
            Ok(Some(head))
          }
          Ok(Admitted::Held) => Ok(replica.head()),
          Err(refusal) => Err(refusal),
        };
        self.send(
          message.to,
          message.from,
          message.shipper,
          Body::Reply(encode_reply(&reply)),
        );
      }
      Body::Reply(bytes) => {
        let reply = decode_reply(&bytes);
        let index = message.shipper;
        if self.owners[index].fenced {
          return;
        }
        // As the daemon drives its shipper (`slates_server::live_ship::fold_into`): the reply is folded, a fence stops
        // the owner, and what is sent next is what `pending` says the holder lacks, not the reply's own answer.
        let next = self.owners[index].shipper.on_reply(message.from, reply);
        if let Next::Fenced(_) = next {
          self.owners[index].fenced = true;
        } else {
          let pending = self.owners[index].shipper.pending(message.from);
          self.send_next(index, message.from, pending);
        }
        self.note_durable(index);
      }
    }
  }

  /// Agreement and StaleNeverCommits, at every durable advance.
  fn note_durable(&mut self, index: usize) {
    let owner = &self.owners[index];
    let Some(durable) = owner.shipper.durable() else {
      return;
    };
    if owner.superseded.is_some() {
      let host = owner.host.0;
      self.note(|| {
        format!("superseded owner {index} (host {host}) is durable through position {durable}")
      });
    }
    for position in 0..=durable {
      let value = owner.state[usize::try_from(position).unwrap()];
      let earlier = *self.durable.entry(position).or_insert(value);
      assert_eq!(
        earlier, value,
        "Agreement: position {position} durable with two values"
      );
      if let Some((adopted, state)) = &owner.superseded {
        let inside = adopted.is_some_and(|head| position <= head.position)
          && state.get(usize::try_from(position).unwrap()) == Some(&value);
        assert!(
          inside,
          "StaleNeverCommits: a superseded owner made position {position} durable outside the adopted head"
        );
      }
    }
  }

  /// Loses the current owner and takes it over: phase one over `f + 1` reachable live candidates under a new epoch.
  fn take_over(&mut self) {
    let old = self.current;
    let old_host = self.owners[old].host;
    let candidates: Vec<HostId> = self
      .hosts
      .iter()
      .copied()
      .filter(|host| *host != old_host && self.holders.get(host).is_some_and(|holder| holder.alive))
      .collect();
    let Some(successor) = candidates.get(self.rng.below(candidates.len())).copied() else {
      return;
    };
    let reachable: Vec<HostId> = candidates
      .iter()
      .copied()
      .filter(|host| {
        !self.cut.contains(&(successor, *host)) && !self.cut.contains(&(*host, successor))
      })
      .collect();
    let needed = usize::try_from(self.f + 1).unwrap();
    if reachable.len() < needed {
      return;
    }
    self.epoch += 1;
    let epoch = HostEpoch(self.epoch);
    let mut reports = Vec::new();
    for host in reachable.iter().take(needed) {
      let holder = self.holders.get_mut(host).unwrap();
      let report = match (&mut holder.replica, self.rule) {
        (Some(replica), _) => replica.prepare(epoch).unwrap(),
        (None, EmptyReport::FenceAndCount) => {
          let mut replica = Replica::new(holder.configured, OBJECT);
          let head = replica.prepare(epoch).unwrap();
          holder.replica = Some(replica);
          self.empty_reports += 1;
          head
        }
        (None, EmptyReport::CountWithoutFence) => {
          self.empty_reports += 1;
          None
        }
      };
      reports.push((*host, report));
    }
    let (epoch_value, reported) = (self.epoch, format!("{reports:?}"));
    self.note(|| {
      format!(
        "takeover at epoch {epoch_value} by host {}: reports {reported}",
        successor.0
      )
    });
    let adopted = adopt(
      &reports,
      Quorum {
        f: u32::try_from(self.f).unwrap(),
      },
    );
    let state = adopted.map_or_else(Vec::new, |(holder, _)| self.holders[&holder].state.clone());
    let head = adopted.map(|(_, head)| head);
    // NoLoss: the adopted state holds every durable entry.
    for (position, value) in &self.durable {
      assert_eq!(
        state.get(usize::try_from(*position).unwrap()),
        Some(value),
        "NoLoss: durable position {position} missing from the head adopted at epoch {}",
        self.epoch
      );
    }
    self.owners[old].superseded = Some((head, state.clone()));
    // The old owner's host is lost for good: its replica role ends with it. Its shipper stays, superseded, and keeps
    // shipping (a paused owner resumed).
    self.lost += 1;
    // Phase one's participants learn the successor at once; the other holders learn it later from the configuration
    // (`learn`), and until then only their fence, if a takeover raised it, stands between them and the superseded
    // owner.
    for host in reachable.iter().take(needed) {
      if let Some(replica) = &mut self.holders.get_mut(host).unwrap().replica {
        replica.reassign(successor);
      }
    }
    // The successor serves from the adopted state; it holds it (as a holder, it fetched it from the adopted replica).
    let successor_holder = self.holders.remove(&successor).unwrap();
    let mut holders: Vec<HostId> = self.holders.keys().copied().collect();
    holders.retain(|host| *host != successor);
    // The old owner was a candidate; it is gone, but stays in the holder list as a dead candidate it ships to.
    holders.push(old_host);
    self.holders.insert(
      old_host,
      Holder {
        replica: None,
        configured: successor,
        state: Vec::new(),
        alive: false,
      },
    );
    drop(successor_holder);
    let shipper = Shipper::new(
      (successor, OBJECT, epoch),
      Quorum {
        f: u32::try_from(self.f).unwrap(),
      },
      &holders,
      head,
      (8, 16),
    );
    self.owners.push(Owner {
      host: successor,
      shipper,
      state,
      superseded: None,
      fenced: false,
    });
    self.current = self.owners.len() - 1;
    self.takeovers += 1;
  }

  /// A holder learns the current configuration: the current owner and its epoch.
  fn learn(&mut self) {
    let owner = self.owners[self.current].host;
    let epoch = HostEpoch(self.epoch);
    let hosts: Vec<HostId> = self.holders.keys().copied().collect();
    if let Some(host) = hosts.get(self.rng.below(hosts.len())) {
      learn_owner(self.holders.get_mut(host).unwrap(), owner, epoch);
    }
  }

  fn crash_holder(&mut self) {
    let owner = self.owners[self.current].host;
    let live: Vec<HostId> = self
      .holders
      .iter()
      .filter(|(host, holder)| holder.alive && **host != owner)
      .map(|(host, _)| *host)
      .collect();
    if let Some(host) = live.get(self.rng.below(live.len())) {
      self.holders.get_mut(host).unwrap().alive = false;
      self.lost += 1;
    }
  }

  fn step(&mut self) {
    let roll = self.rng.below(100);
    match roll {
      0..=29 => self.write(self.current),
      30..=69 => {
        if !self.network.is_empty() {
          let at = self.rng.below(self.network.len());
          self.deliver(at);
        }
      }
      70..=74 => {
        if !self.network.is_empty() {
          let at = self.rng.below(self.network.len());
          self.network.swap_remove(at);
        }
      }
      75..=78 => {
        if !self.network.is_empty() {
          let at = self.rng.below(self.network.len());
          let copy = self.network[at].clone();
          self.network.push(copy);
        }
      }
      79..=83 => self.tick(self.current),
      84..=86 => {
        let a = self.hosts[self.rng.below(self.hosts.len())];
        let b = self.hosts[self.rng.below(self.hosts.len())];
        if a != b && !self.cut.remove(&(a, b)) {
          self.cut.insert((a, b));
        }
      }
      87..=89 => {
        // A superseded owner keeps writing and re-shipping.
        let stale: Vec<usize> = (0..self.owners.len())
          .filter(|index| *index != self.current)
          .collect();
        if let Some(index) = stale.get(self.rng.below(stale.len())).copied() {
          if self.rng.chance(500) {
            self.write(index);
          } else {
            self.tick(index);
          }
        }
      }
      90..=91 if self.lost < self.f => self.take_over(),
      93..=95 => self.learn(),
      92 if self.lost < self.f => self.crash_holder(),
      _ => {}
    }
  }

  /// Heals the network and drives the current owner until every live holder holds its state.
  fn settle(&mut self) {
    self.cut.clear();
    self.everyone_learns();
    // Messages of superseded owners are still delivered: they must change nothing.
    for _ in 0..10_000 {
      if self.network.is_empty() {
        self.tick(self.current);
        if self.network.is_empty() {
          break;
        }
      }
      let at = self.rng.below(self.network.len());
      self.deliver(at);
      if self.converged() {
        break;
      }
    }
    assert!(
      self.converged(),
      "Convergence: live holders never caught up"
    );
    let owner = &self.owners[self.current];
    for (position, value) in &self.durable {
      assert_eq!(
        owner.state.get(usize::try_from(*position).unwrap()),
        Some(value),
        "NoLoss at the end: durable position {position}"
      );
    }
  }

  /// Every holder learns the current configuration, as the configuration group's commit reaches each in the end.
  fn everyone_learns(&mut self) {
    let owner = self.owners[self.current].host;
    let epoch = HostEpoch(self.epoch);
    for holder in self.holders.values_mut() {
      learn_owner(holder, owner, epoch);
    }
  }

  fn converged(&self) -> bool {
    let owner = &self.owners[self.current];
    self
      .holders
      .iter()
      .filter(|(host, holder)| holder.alive && **host != owner.host)
      .all(|(_, holder)| {
        holder.state == owner.state
          && holder.replica.as_ref().and_then(Replica::head) == owner.shipper.head()
      })
  }
}

fn run_seeds(f: u64, rule: EmptyReport) -> (u64, u64, u64, u64, u64) {
  let mut totals = (0, 0, 0, 0, 0);
  for seed in 0..SEEDS {
    let mut run = Run::new(f, seed, rule);
    for _ in 0..STEPS {
      run.step();
    }
    run.settle();
    totals.0 += run.takeovers;
    totals.1 += run.installs;
    totals.2 += run.stale_refusals;
    totals.3 += u64::try_from(run.durable.len()).unwrap();
    totals.4 += run.empty_reports;
  }
  totals
}

/// A-200, AC-8.3: under loss, duplication, reordering, partitions, holder crashes, owner loss with takeover and a
/// superseded owner shipping on, at `f = 1` and `f = 2`, every history holds Agreement, NoLoss, StaleNeverCommits and
/// Convergence. The counters are the non-vacuity check: takeovers happened, installs caught holders up, superseded
/// owners were refused by fences, and writes became durable.
#[test]
fn the_live_log_holds_the_register_invariants_under_the_nemesis() {
  for f in [1, 2] {
    let (takeovers, installs, stale_refusals, durable, empty) =
      run_seeds(f, EmptyReport::FenceAndCount);
    eprintln!(
      "f={f}: {SEEDS} seeds x {STEPS} steps: {takeovers} takeovers, {installs} installs, {stale_refusals} stale \
       refusals, {durable} durable positions, {empty} empty reports counted"
    );
    assert!(takeovers > 0 && installs > 0 && stale_refusals > 0 && durable > 0 && empty > 0);
  }
}

/// A-200 phase one, the rule for a candidate that holds no copy (2026-11-05): its empty report counts toward `f + 1`
/// only because it raises a fence. Do run the nemesis with the mutation that counts it and keeps no fence (the daemon's
/// remote no-copy answer before the fix). Expect a superseded owner to make a position durable through that candidate,
/// outside the adopted head: StaleNeverCommits fails in some seed, at `f = 1` or `f = 2`.
#[test]
fn an_empty_report_without_a_fence_lets_a_superseded_owner_commit() {
  let mut broken = Vec::new();
  for f in [1, 2] {
    for seed in 0..SEEDS {
      let outcome = std::panic::catch_unwind(|| {
        let mut run = Run::new(f, seed, EmptyReport::CountWithoutFence);
        for _ in 0..STEPS {
          run.step();
        }
        run.settle();
      });
      if let Err(panic) = outcome {
        let message = panic
          .downcast_ref::<String>()
          .cloned()
          .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
          .unwrap_or_default();
        broken.push((f, seed, message));
      }
    }
  }
  eprintln!(
    "empty reports without a fence: {} broken seeds; first {:?}",
    broken.len(),
    broken.first()
  );
  // The first broken seed's history, for its record (docs/bugs/2026-11-05-…): replayed with the trace on.
  if let Some((f, seed, _)) = broken.first().cloned() {
    eprintln!("history of f={f} seed {seed}:");
    let _ = std::panic::catch_unwind(|| {
      let mut run = Run::new(f, seed, EmptyReport::CountWithoutFence).traced();
      for _ in 0..STEPS {
        run.step();
      }
      run.settle();
    });
  }
  assert!(
    broken
      .iter()
      .any(|(_, _, message)| message.contains("StaleNeverCommits")),
    "an unfenced empty report let a superseded owner commit: {broken:?}"
  );
}

/// AC-8.9: the owner holds at most `credit` entries that are not durable; past it a write is refused `NoCredit`, and
/// an acknowledgement gives the credit back.
#[test]
fn a_writer_past_its_credit_waits_for_acknowledgements() {
  let hosts = [HostId(1), HostId(2), HostId(3)];
  let mut shipper = Shipper::new(
    (hosts[0], OBJECT, HostEpoch(1)),
    Quorum { f: 1 },
    &hosts,
    None,
    (2, 4),
  );
  let first = shipper.append(vec![1]).unwrap();
  let second = shipper.append(vec![2]).unwrap();
  assert_eq!(
    shipper.append(vec![3]),
    Err(LiveError::NoCredit { durable: None })
  );
  let mut replica = Replica::new(hosts[0], OBJECT);
  replica.accept(&first).unwrap();
  let held = replica.accept(&second).unwrap();
  assert_eq!(shipper.on_reply(hosts[1], Ok(held)), Next::Nothing);
  assert_eq!(shipper.durable(), Some(1));
  assert!(shipper.append(vec![3]).is_ok());
}

/// A-200, AC-8.3: a holder's refusal is never counted as holding. Do append two entries, then answer the owner with
/// each refusal a holder that will not keep them gives (a sender it does not take for the owner, no catalog yet, a
/// fence, a stranger's object, malformed bytes). Expect the durable position unchanged after every one, and the owner
/// still owing that holder both entries; only the other holder's acknowledgement makes them durable.
#[test]
fn a_refused_entry_is_never_counted_as_held() {
  let hosts = [HostId(1), HostId(2), HostId(3)];
  let mut shipper = Shipper::new(
    (hosts[0], OBJECT, HostEpoch(1)),
    Quorum { f: 1 },
    &hosts,
    None,
    (4, 8),
  );
  let first = shipper.append(vec![1]).unwrap();
  let second = shipper.append(vec![2]).unwrap();
  for refusal in [
    LiveError::Unauthorized,
    LiveError::ForeignObject,
    LiveError::Malformed,
    LiveError::NoCredit { durable: None },
    LiveError::StaleEpoch {
      current: HostEpoch(1),
    },
  ] {
    let next = shipper.on_reply(hosts[1], Err(refusal));
    assert_ne!(
      shipper.durable(),
      Some(1),
      "{refusal:?} made the log durable"
    );
    assert_eq!(shipper.durable(), None, "{refusal:?} moved durability");
    assert!(
      !matches!(next, Next::Fenced(_)),
      "a refusal at the owner's own epoch fences nothing: {refusal:?}"
    );
    assert_eq!(
      shipper.pending(hosts[1]),
      Next::Entries(vec![first.clone(), second.clone()]),
      "after {refusal:?} the owner still owes the holder both entries"
    );
  }
  let mut replica = Replica::new(hosts[0], OBJECT);
  replica.accept(&first).unwrap();
  let held = replica.accept(&second).unwrap();
  shipper.on_reply(hosts[2], Ok(held));
  assert_eq!(
    shipper.durable(),
    Some(1),
    "the other holder's acknowledgement does"
  );
}

/// R8: at `f = 0` there are no holders and every entry is durable at its append, by the same code.
#[test]
fn the_laptop_is_durable_at_its_own_append() {
  let mut shipper = Shipper::new(
    (HostId(1), OBJECT, HostEpoch(1)),
    Quorum { f: 0 },
    &[HostId(1)],
    None,
    (1, 1),
  );
  for value in 0..100u8 {
    let entry = shipper.append(vec![value]).unwrap();
    assert_eq!(shipper.durable(), Some(entry.position));
  }
}

/// Hostile input: every truncation, a payload over the cap, a length that over-claims, and every single-bit flip of an
/// entry either decodes to a different entry or is refused, never a panic.
#[test]
fn hostile_bytes_are_refused_not_trusted() {
  let entry = Entry {
    owner: HostId(1),
    object: OBJECT,
    position: 3,
    epoch: HostEpoch(2),
    previous: [9; 32],
    payload: vec![1, 2, 3, 4],
  };
  let bytes = entry.encode();
  assert_eq!(Entry::decode(&bytes, CAP), Ok(entry.clone()));
  for length in 0..bytes.len() {
    assert_eq!(
      Entry::decode(&bytes[..length], CAP),
      Err(LiveError::Malformed)
    );
  }
  assert_eq!(Entry::decode(&bytes, 3), Err(LiveError::Malformed));
  let mut over = bytes.clone();
  let at = over.len() - 4 - 4;
  over[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
  assert_eq!(Entry::decode(&over, CAP), Err(LiveError::Malformed));
  for bit in 0..bytes.len() * 8 {
    let mut flipped = bytes.clone();
    flipped[bit / 8] ^= 1 << (bit % 8);
    if let Ok(decoded) = Entry::decode(&flipped, CAP) {
      assert_ne!(decoded.identity(), entry.identity());
    }
  }
}

/// Hostile input: a reply that is empty, of an unknown kind, short of its head or fence, or longer than its kind is
/// refused.
#[test]
fn hostile_replies_are_refused() {
  for reply in [vec![], vec![9], vec![1, 0], vec![3, 1, 2], vec![2, 0]] {
    assert_eq!(decode_reply(&reply), Err(LiveError::Malformed));
  }
}

/// Hostile input on the header readers a holder uses before it decodes a message it will not serve: an entry's and an
/// install's epoch are read back as encoded, every truncation short of the epoch reads none, and a fetch is not read as
/// an install. Never a panic.
#[test]
fn a_messages_epoch_is_read_from_its_header_alone() {
  use slates_db::live::{Fetch, epoch_of, epoch_of_install};
  let entry = Entry {
    owner: HostId(1),
    object: OBJECT,
    position: 3,
    epoch: HostEpoch(5),
    previous: [9; 32],
    payload: vec![1, 2, 3, 4],
  };
  let bytes = entry.encode();
  assert_eq!(epoch_of(&bytes), Some(HostEpoch(5)));
  let header = 8 + 16 + 8 + 8;
  for length in 0..header {
    assert_eq!(epoch_of(&bytes[..length]), None);
  }
  assert_eq!(epoch_of(&bytes[..header]), Some(HostEpoch(5)));
  let install = Install {
    owner: HostId(1),
    object: OBJECT,
    epoch: HostEpoch(6),
    head: Head {
      epoch: HostEpoch(6),
      position: 0,
      identity: [3; 32],
    },
    state: vec![8; 12],
  };
  let bytes = install.encode();
  assert_eq!(epoch_of_install(&bytes), Some(HostEpoch(6)));
  let header = 1 + 8 + 16 + 8;
  for length in 0..header {
    assert_eq!(epoch_of_install(&bytes[..length]), None);
  }
  let fetch = Fetch {
    object: OBJECT,
    epoch: HostEpoch(7),
  };
  assert_eq!(epoch_of_install(&fetch.encode()), None);
}

/// Hostile input on the whole-state kind: a fetch round-trips; every truncation of it, an install tag where a fetch is
/// read, and a fetch with bytes after it are refused, never a panic.
#[test]
fn hostile_fetches_are_refused() {
  use slates_db::live::{Fetch, is_fetch, object_of_state};
  let fetch = Fetch {
    object: OBJECT,
    epoch: HostEpoch(3),
  };
  let bytes = fetch.encode();
  assert!(is_fetch(&bytes));
  assert_eq!(object_of_state(&bytes), Some(OBJECT));
  assert_eq!(Fetch::decode(&bytes), Ok(fetch));
  for length in 0..bytes.len() {
    assert_eq!(Fetch::decode(&bytes[..length]), Err(LiveError::Malformed));
  }
  let mut longer = bytes.clone();
  longer.push(0);
  assert_eq!(Fetch::decode(&longer), Err(LiveError::Malformed));
  let mut install_tag = bytes.clone();
  install_tag[0] = 1;
  assert_eq!(Fetch::decode(&install_tag), Err(LiveError::Malformed));
}

/// Hostile input on a fetched reply: it round-trips; a payload over the cap is refused, and no cut of it decodes whole.
#[test]
fn hostile_fetched_replies_are_refused() {
  use slates_db::live::{decode_fetched, encode_fetched};
  let head = Head {
    epoch: HostEpoch(2),
    position: 7,
    identity: [4; 32],
  };
  let payloads = vec![vec![1, 2, 3], vec![9; 40]];
  let reply = encode_fetched(&Ok(Some(head)), &payloads);
  assert_eq!(
    decode_fetched(&reply, CAP),
    Ok((Some(head), payloads.clone()))
  );
  assert_eq!(decode_fetched(&reply, 8), Err(LiveError::Malformed));
  for length in 0..reply.len() {
    if let Ok((_, decoded)) = decode_fetched(&reply[..length], CAP) {
      assert!(
        decoded.len() < payloads.len(),
        "a cut reply never decodes whole"
      );
    }
  }
}
