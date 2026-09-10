//! The owner runtime on one node (§4.8; the design's boot step 6, §2.6; D-14): the single object the
//! control shard holds to take part in a region — the SWIM membership view, the configuration group,
//! and the node's own register acceptor, kept in step. Boot step 6 names exactly this composition —
//! "the control shard joins membership, learns its neighbourhood and host epoch from the regional
//! configuration, and begins replication to its candidate holders" — and the laptop is its `f = 0`
//! degenerate: one member, one voter, the owner's local hold the commit, the *same code path* as a
//! fleet (R8), never a mode switch.
//!
//! The ownership split is unchanged: the register protocol (`slates-db`) owns synchronous acceptance;
//! the async commit/promote drivers of [`crate`] own the transport dispatch; this composes them with
//! the failure detector's view ([`crate::membership`]) and the configuration authority
//! ([`crate::config_group`]). This module is the **synchronous** runtime — it turns a membership event
//! into the right configuration action and keeps the owner's acceptor authority in step with the
//! configuration version — so the async drivers borrow [`FleetNode::configuration`] and
//! [`FleetNode::owner_acceptor`] to ship records and prepares over the wire against a consistent
//! authority. Keeping the three in step is the real work here: a neighbourhood change advances the
//! configuration version, and the owner must write its next records under that version (a holder that
//! installed the new configuration refuses an older-generation record), so the owner's own acceptor
//! adopts the new authority the moment the configuration moves.
//!
//! Scope (this piece): the authority core and its N=1-vs-fleet differential (R8).
//!
//! **Wired into the daemon (2026-09-10):** `slates-server` depends on `slates-cluster`, and each shard's
//! `ShardState` holds a `FleetNode` (`FleetNode::solo` at boot — the laptop `f = 0`). The daemon's
//! placement authority — every `place`/`region_placed`/`await_placed`/`host_epoch` the verbs read — now
//! comes from `fleet.configuration()`, so the register/placement path runs the fleet's configuration
//! group rather than a bare `Configuration` (R8: the same code the fleet runs, degenerate at N=1). Verb
//! and daemon-lifecycle behaviour is unchanged at N=1.
//!
//! Deliberately **not** here yet, and owed as the next pieces, each at a real boundary:
//!
//! - the object→owner routing registry — *which* of a dead host's objects this node holds — so
//!   cross-node takeover can be driven per object (the `ConfigGroup`'s single-owner `Configuration`
//!   models this node's authority over its own objects, not a per-object table);
//! - the live probe/gossip loop that feeds [`observe`](FleetNode::observe) from the [`crate::detector`],
//!   and the async register lifecycle (commit a head, on takeover run the promotion) driven from it;
//! - propagating a control-shard membership change to the worker shards' configurations (at N=1 there
//!   are none, so each shard's solo `FleetNode` agrees; a fleet's control shard `observe`s and the new
//!   configuration must reach the shards that place objects).
//!
//! Those are transport- and server-layer pieces; this authority core is confirmable on its own.

use slates_db::register::{Acceptor, Authority, Configuration, HostId, Quorum, Record};
use slates_transport::endpoint::Endpoint;

use crate::config_group::{ConfigGroup, Reconfiguration};
use crate::membership::{Liveness, MemberState, Membership};
use crate::{CommitBudget, Committed, commit_under_configuration};

/// The owner runtime on one node: the SWIM view, the configuration authority, and the owner's own
/// register acceptor, composed and kept in step. Built at a fault tolerance (`f = 0` is the laptop);
/// [`observe`](FleetNode::observe) folds a membership event into the configuration and the acceptor's
/// authority; [`configuration`](FleetNode::configuration) and [`owner_acceptor`](FleetNode::owner_acceptor)
/// are what the async register drivers consume.
pub struct FleetNode {
  host: HostId,
  membership: Membership,
  group: ConfigGroup,
  acceptor: Acceptor,
}

impl FleetNode {
  /// A fleet node at fault tolerance `quorum`, this `host` the owner, with `peers` initially believed
  /// alive. The configuration group starts single-owner at `quorum` and is reconciled to the alive
  /// membership at once, so the neighbourhood already holds `host` and every peer. The owner's acceptor
  /// serves under the resulting authority (generation = the configuration version, owner = this host).
  /// `solo` is this with no peers at `f = 0` — the laptop, the same code path.
  pub fn new(host: HostId, quorum: Quorum, peers: &[HostId]) -> FleetNode {
    let mut membership = Membership::new(host);
    for &peer in peers {
      membership.apply(
        peer,
        MemberState {
          liveness: Liveness::Alive,
          incarnation: 0,
        },
      );
    }
    let mut group = ConfigGroup::new(host, quorum);
    group.reconcile(&membership);
    let acceptor = Acceptor::new(host, Self::authority(group.configuration()));
    FleetNode {
      host,
      membership,
      group,
      acceptor,
    }
  }

  /// The laptop node — one member, one voter, `f = 0`, the owner's local hold the commit. The `f = 0`
  /// degenerate of [`new`](FleetNode::new), the same code path a fleet runs (R8).
  pub fn solo(host: HostId) -> FleetNode {
    FleetNode::new(host, Quorum { f: 0 }, &[])
  }

  /// This node's host id — the owner of the objects it serves and the writer of their registers.
  pub fn host(&self) -> HostId {
    self.host
  }

  /// The current configuration (the authority the async register drivers carry): the owner, the host
  /// epoch, the neighbourhood the candidates are drawn from, the quorum, and the version a request
  /// carries. Read per request; written only through the membership events [`observe`](FleetNode::observe)
  /// folds in.
  pub fn configuration(&self) -> &Configuration {
    self.group.configuration()
  }

  /// The SWIM membership view — the alive set the neighbourhood tracks, and the state the live probe
  /// loop (owed) reads and gossips.
  pub fn membership(&self) -> &Membership {
    &self.membership
  }

  /// The owner's own register acceptor — its local hold of the heads it owns. The async
  /// [`commit_record`](crate::commit_record) borrows this as the owner's candidate hold; it serves
  /// under the authority [`observe`](FleetNode::observe) keeps in step with the configuration.
  pub fn owner_acceptor(&mut self) -> &mut Acceptor {
    &mut self.acceptor
  }

  /// The authority an owner's acceptor serves under for the given configuration: the configuration
  /// version is the generation records are written under, and this host is the authorized owner.
  fn authority(configuration: &Configuration) -> Authority {
    Authority {
      generation: configuration.version,
      owner: configuration.owner,
    }
  }

  /// Folds a gossiped membership `update` about `subject` into the runtime (§4.8 "membership fed by
  /// SWIM"): applies it to the view, and if the view changed, reconciles the configuration's
  /// neighbourhood to the new alive set (admitting a fresh member, retiring a dead one). When the
  /// configuration version advances, the owner's own acceptor adopts the new authority — so the owner
  /// writes its next records under the current version, which a holder that installed the new
  /// configuration requires. Returns whether the configuration changed.
  ///
  /// At `f = 0` there are no peers to hear about, so `observe` is only ever a self-refutation (a no-op
  /// for the neighbourhood) — the same code path, exercised trivially, that a fleet drives with real
  /// peers. It never removes this host (the owner is never retired), so the runtime always has an owner.
  pub fn observe(&mut self, subject: HostId, update: MemberState) -> bool {
    if self.membership.apply(subject, update).is_none() {
      return false;
    }
    let changed = self.group.reconcile(&self.membership);
    if changed {
      // The version moved; keep the owner's acceptor authority in step (install_authority accepts an
      // equal-or-higher generation, so a monotonically advancing version is always adopted, keeping the
      // owner writing under the current generation). The owner is unchanged, so this never fences the
      // owner's own committed records — it raises the generation future records are written under.
      let authority = Self::authority(self.group.configuration());
      let _ = self.acceptor.install_authority(authority);
    }
    changed
  }

  /// Reconfigures the neighbourhood directly (an operator admit/retire, not a SWIM-driven change),
  /// keeping the owner's acceptor authority in step exactly as [`observe`](FleetNode::observe) does.
  /// Returns whether the configuration changed.
  pub fn reconfigure(&mut self, change: Reconfiguration) -> bool {
    let changed = self.group.reconfigure(change);
    if changed {
      let authority = Self::authority(self.group.configuration());
      let _ = self.acceptor.install_authority(authority);
    }
    changed
  }

  /// Commits one of this node's own heads through the register path under the current authority — the
  /// owner runtime's write operation (§4.8 "records are sent to all candidates; committed at `f + 1`").
  /// The owner holds the record locally through its own acceptor; each `remote_holder` (a connected
  /// [`Endpoint`] per remaining candidate) is shipped it concurrently, and the commit places at `f + 1`
  /// distinct acknowledgements or reports uncertain at the deadline — the [`commit_under_configuration`]
  /// dispatch, driven against *this* node's configuration and acceptor so authority and dispatch cannot
  /// diverge. At `f = 0` the local hold is the commit, no dispatch, the same code path (R8).
  ///
  /// This borrows the configuration (`&self.group`) and the acceptor (`&mut self.acceptor`) — disjoint
  /// fields — so no clone is needed on the write path; the configuration is read, not copied, per
  /// commit. The caller supplies the connected candidate holders (the live probe/gossip loop that keeps
  /// them connected is owed) and the derived budget.
  pub async fn commit_head(
    &mut self,
    record: &Record,
    remote_holders: Vec<(HostId, Endpoint)>,
    budget: CommitBudget,
  ) -> Committed {
    commit_under_configuration(
      self.group.configuration(),
      record,
      &mut self.acceptor,
      remote_holders,
      budget,
    )
    .await
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::register::{ObjectId, Placement, Record, commit_over_holders};

  const SELF: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);

  fn alive(incarnation: u64) -> MemberState {
    MemberState {
      liveness: Liveness::Alive,
      incarnation,
    }
  }

  fn dead(incarnation: u64) -> MemberState {
    MemberState {
      liveness: Liveness::Dead,
      incarnation,
    }
  }

  /// A dedicated probe position the invariant helper writes, distinct from any object a test commits
  /// for real, so the probe never conflicts with a test's own head. A constant value at a constant
  /// position means a re-probe under the current generation is an idempotent re-accept, not a conflict.
  const PROBE_OBJECT: ObjectId = ObjectId::new(SELF, u64::MAX);

  /// Whether the owner's acceptor authorizes a head written under `generation` at this node's current
  /// authority — driven by use: a record under the configuration's version is accepted, one under a
  /// stale generation is refused. This is how the test reads "the acceptor authority is in step".
  fn owner_accepts_under(node: &mut FleetNode, generation: u64) -> bool {
    let owner = node.configuration().owner;
    let epoch = node.configuration().host_epoch;
    let candidates = vec![owner];
    let record = Record {
      owner,
      object: PROBE_OBJECT,
      sequence: 0,
      epoch,
      generation,
      value: b"probe".to_vec(),
    };
    let owner_holder = node.owner_acceptor();
    let mut refs: Vec<&mut dyn slates_db::register::Holder> = vec![owner_holder];
    let placement = commit_over_holders(&candidates, &record, &mut refs);
    !placement.acked.is_empty()
  }

  /// The invariants the owner runtime holds at *every* scale (checked identically at N=1 and in a
  /// fleet — the R8 differential): the owner is a live member; the neighbourhood is exactly the alive
  /// set; and the owner's acceptor authorizes a head under the configuration's current version (its
  /// authority is in step) but refuses one under a stale generation.
  fn assert_runtime_invariants(node: &mut FleetNode) {
    let owner = node.configuration().owner;
    let version = node.configuration().version;
    let neighbourhood = node.configuration().neighbourhood.clone();
    let alive = node.membership().alive();

    assert!(alive.contains(&owner), "the owner is a live member");
    let mut sorted_neighbourhood = neighbourhood.clone();
    sorted_neighbourhood.sort_by_key(|h| h.0);
    let mut sorted_alive = alive.clone();
    sorted_alive.sort_by_key(|h| h.0);
    assert_eq!(
      sorted_neighbourhood, sorted_alive,
      "the neighbourhood is exactly the alive set"
    );
    assert!(
      owner_accepts_under(node, version),
      "the owner acceptor authorizes a head under the current version (authority in step)"
    );
    if version > 0 {
      assert!(
        !owner_accepts_under(node, version - 1),
        "the owner acceptor refuses a head under a stale generation"
      );
    }
  }

  /// AC (§4.8 laptop degenerate, R8): the solo runtime is one member, one voter, `f = 0`, its own
  /// owner; a head commits on its local hold alone; and the invariants hold — the same ones the fleet
  /// holds.
  #[test]
  fn the_solo_runtime_owns_itself_and_commits_locally() {
    let mut node = FleetNode::solo(SELF);
    assert_eq!(
      node.configuration().owner,
      SELF,
      "the node owns its objects"
    );
    assert_eq!(
      node.configuration().neighbourhood,
      vec![SELF],
      "the laptop neighbourhood is itself"
    );
    assert_eq!(node.configuration().quorum, Quorum { f: 0 });

    // A head commits on the local hold — the owner is f+1 at f=0.
    let record = Record {
      owner: SELF,
      object: ObjectId::new(SELF, 0),
      sequence: 0,
      epoch: node.configuration().host_epoch,
      generation: node.configuration().version,
      value: b"head@v1".to_vec(),
    };
    let candidates = vec![SELF];
    let placement = {
      let owner_holder = node.owner_acceptor();
      let mut refs: Vec<&mut dyn slates_db::register::Holder> = vec![owner_holder];
      commit_over_holders(&candidates, &record, &mut refs)
    };
    assert!(
      placement.placed(Quorum { f: 0 }),
      "the head commits on the local hold at f=0: {placement:?}"
    );

    assert_runtime_invariants(&mut node);
  }

  /// AC (§4.8): the runtime folds a SWIM view into the configuration — a fresh alive member grows the
  /// neighbourhood and advances the version; a death retires it; and the owner's acceptor authority
  /// stays in step across each change (checked by the invariants after every step).
  #[test]
  fn the_runtime_tracks_the_swim_view_and_keeps_authority_in_step() {
    let mut node = FleetNode::new(SELF, Quorum { f: 1 }, &[]);
    assert_runtime_invariants(&mut node);
    let v0 = node.configuration().version;

    // A joins: the neighbourhood grows, the version advances, the authority stays in step.
    assert!(
      node.observe(A, alive(0)),
      "a fresh member changes the config"
    );
    assert!(node.configuration().neighbourhood.contains(&A));
    assert!(node.configuration().version > v0, "the version advanced");
    assert_runtime_invariants(&mut node);

    // B joins likewise.
    assert!(node.observe(B, alive(0)));
    assert_runtime_invariants(&mut node);
    let with_both = node.configuration().version;

    // A stale re-assertion of A (lower/equal incarnation, already alive) changes nothing.
    assert!(!node.observe(A, alive(0)), "a stale update is a no-op");
    assert_eq!(node.configuration().version, with_both, "no version churn");

    // A dies: it is retired from the neighbourhood, the version advances, the authority stays in step.
    assert!(node.observe(A, dead(1)), "a death changes the config");
    assert!(
      !node.configuration().neighbourhood.contains(&A),
      "the dead member is retired"
    );
    assert_runtime_invariants(&mut node);
  }

  /// AC (R8, the named differential): the owner runtime has identical observable semantics at N=1 and
  /// in a simulated fleet. The **same** code, driven with no peers (`f = 0`) and with two peers
  /// (`f = 1`), holds the same invariants after the same shape of history — there is no mode switch,
  /// only a different N in one formula family.
  #[test]
  fn the_owner_runtime_has_identical_semantics_at_n1_and_in_a_fleet() {
    // N=1: the laptop. No peers ever arrive; the invariants hold throughout.
    let mut laptop = FleetNode::solo(SELF);
    assert_runtime_invariants(&mut laptop);
    // The only membership event possible at N=1 is about the local node; it never changes the
    // neighbourhood (self is never retired), and the invariants still hold.
    laptop.observe(SELF, alive(1));
    assert_runtime_invariants(&mut laptop);
    assert_eq!(laptop.configuration().neighbourhood, vec![SELF]);

    // Fleet: the same runtime with two peers. The invariants hold at construction and after each
    // membership event — the identical assertions the laptop passed.
    let mut fleet = FleetNode::new(SELF, Quorum { f: 1 }, &[A, B]);
    assert_runtime_invariants(&mut fleet);
    fleet.observe(A, dead(1));
    assert_runtime_invariants(&mut fleet);
    fleet.observe(B, dead(1));
    assert_runtime_invariants(&mut fleet);
    // Every peer gone, the fleet has degenerated to exactly the laptop's observable configuration:
    // one member, itself the owner — reached by the same code, no mode switch.
    assert_eq!(
      fleet.configuration().neighbourhood,
      vec![SELF],
      "with every peer dead the fleet configuration equals the laptop's"
    );
    assert_eq!(fleet.configuration().owner, laptop.configuration().owner);
  }

  /// A guard that the differential is not vacuous: the fleet path genuinely exercised a multi-member
  /// neighbourhood before degenerating (so "identical semantics" is not passing merely because the
  /// fleet never differed from N=1).
  #[test]
  fn the_fleet_path_genuinely_grows_before_it_degenerates() {
    let fleet = FleetNode::new(SELF, Quorum { f: 1 }, &[A, B]);
    let grown: Placement = fleet.configuration().place(ObjectId::new(SELF, 0));
    assert_eq!(
      grown.candidates.len(),
      3,
      "at f=1 with two peers the object has three candidate holders — a real fleet topology"
    );
  }
}
