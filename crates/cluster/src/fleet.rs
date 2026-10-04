//! The owner runtime on one node (§4.8; the design's boot step 6, §2.6; D-14): the single object the
//! control shard holds to take part in a region — the SWIM membership view, this node's **current
//! configuration** (what the regional council has agreed and this node places under), and the node's own
//! register acceptor, kept in step. Boot step 6 names exactly this composition — "the control shard joins
//! membership, learns its neighbourhood and host epoch from the regional configuration, and begins
//! replication to its candidate holders" — and the laptop is its `f = 0` degenerate: one member, a solo
//! council that self-leads, the owner's local hold the commit, the *same code path* as a fleet (R8), never
//! a mode switch.
//!
//! **The configuration authority is the regional council, not this module** (D-14, one configuration group
//! per region): the council ([`crate::config_group::RegionalCouncil`]) reconciles the region's membership,
//! neighbourhoods and host epochs by consensus over the transport; every node **installs** the committed
//! configuration ([`install_configuration`](FleetNode::install_configuration)) and reads it per request
//! ([`configuration`](FleetNode::configuration)). This module composes that installed configuration with the
//! failure detector's view ([`crate::membership`]) and the owner's acceptor, and keeps the acceptor's
//! authority in step with the installed version — a holder on the new configuration refuses an
//! older-generation record, so the owner must write under the current version.
//!
//! The split of work: the register protocol (`slates-db`) owns synchronous acceptance; the async
//! commit/promote drivers of [`crate`] own the transport dispatch; the council owns the configuration; this
//! composes them. [`observe`](FleetNode::observe) folds a SWIM event into the membership view — the failure
//! view the council leader reconciles the configuration from ([`RegionalCouncil::reconcile_alive`](crate::config_group::RegionalCouncil::reconcile_alive)),
//! not a local reconcile. [`install_configuration`](FleetNode::install_configuration) installs a committed
//! configuration and hands back the objects a departed owner's retirement gives this node to take over (the
//! per-object routing view, [`crate::routing`], computes the rendezvous winner over the new neighbourhood).
//! [`apply_peer_state`] folds one peer's belief from the membership plane's detector (`crate::member_plane`) into the
//! membership; the daemon applies it under its admission rule on every shard (A-67 H-2).
//!
//! **Driven live by the daemon** (`slates-server`): each shard's `ShardState` holds a `FleetNode`; the
//! control-shard record-plane coordinator drives the council over the transport and installs its committed
//! configuration into the `FleetNode` each period, so every `place`/`region_placed`/`await_placed`/`host_epoch`
//! the verbs read comes from what the council agreed. At `f = 0` the solo council self-leads and its formed
//! region is authoritative at once — the same code the fleet runs, degenerate at N=1 (R8).

use std::collections::BTreeMap;

use slates_db::register::{
  Acceptor, Authority, Configuration, HostId, ObjectId, Quorum, Record, RegionalConfiguration,
  RegisterError,
};
use slates_transport::endpoint::Endpoint;

use crate::membership::{Liveness, MemberState, Membership};
use crate::routing::Routing;
use crate::{CommitBudget, Committed, commit_under_configuration};

/// The owner runtime on one node: the SWIM view, this node's **current configuration** (the view the
/// configuration council has agreed on and this node placed under), and the owner's own register acceptor,
/// composed and kept in step. The configuration is no longer reconciled here — the regional council
/// ([`crate::config_group::RegionalCouncil`], one per region, D-14) is the authority; this node
/// **installs** the committed configuration the council produces ([`install_configuration`](FleetNode::install_configuration))
/// and reads it per request ([`configuration`](FleetNode::configuration)). [`observe`](FleetNode::observe)
/// folds a SWIM membership event into the view (the leader reconciles the council from it); the async
/// register drivers consume [`configuration`](FleetNode::configuration) and
/// [`owner_acceptor`](FleetNode::owner_acceptor). At `f = 0` the council is the sole voter and the installed
/// configuration is the formed laptop region — the same code path a fleet runs (R8).
pub struct FleetNode {
  host: HostId,
  membership: Membership,
  configuration: Configuration,
  /// The region's members as of the last installed configuration — the whole region, not this node's
  /// neighbourhood. A takeover triggers on a member **leaving the region** (a retirement/death), read by
  /// diffing this against the newly installed configuration's members; a host merely re-ranked out of this
  /// owner's bounded neighbourhood (which can happen once the scatter width exceeds the member count) is
  /// still alive and is **not** taken over.
  members: Vec<HostId>,
  acceptor: Acceptor,
  routing: Routing,
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
    let mut members = vec![host];
    members.extend_from_slice(peers);
    members.sort_unstable_by_key(|host| host.0);
    members.dedup();
    let configuration = Self::owner_view(host, quorum, &members);
    let acceptor = Acceptor::new(host, Self::authority(&configuration));
    FleetNode {
      host,
      membership,
      configuration,
      members,
      acceptor,
      routing: Routing::new(),
    }
  }

  /// The owner's view of the region `members` at `quorum`, each neighbourhood bounded to the candidate floor
  /// `2f+1` (the lowest-loss default the council raises once a deployment sizes its recovery) — the initial
  /// configuration this node places under before the council commits anything. The daemon installs the
  /// council's committed configuration over this at boot
  /// ([`install_configuration`](FleetNode::install_configuration)); a standalone node (a laptop, a test)
  /// keeps it — the `f = 0` degenerate is the region `{host}` alone.
  fn owner_view(host: HostId, quorum: Quorum, members: &[HostId]) -> Configuration {
    let scatter = u64::try_from(quorum.candidates()).unwrap_or(u64::MAX);
    RegionalConfiguration::formed(members.to_vec(), quorum, BTreeMap::new(), scatter, false)
      .configuration_for(host)
      .unwrap_or_else(|| Configuration::solo(host))
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

  /// Installs the configuration the regional council has committed for this owner, given the region's
  /// `members` it was derived from (§4.8, D-14 — the council is the authority; this node places under the
  /// configuration it agreed). Sets it as this node's current configuration and brings the owner's acceptor
  /// authority into step (the generation is the configuration's version, so the owner writes its next records
  /// under the current generation, which a holder on the new configuration requires). Who takes a retired
  /// owner's objects over is the configuration's to name ([`RegionalConfiguration::lineage`]); the node that
  /// installs it moves each held object to its successor ([`track_object_owner`](FleetNode::track_object_owner)).
  /// Idempotent, so the daemon may call it every period.
  ///
  /// This node's **own** fencing epoch advanced: the council took this host over while it was away (retired,
  /// then re-admitted under the same id — a false death, or an isolation that healed). A retired id is
  /// admitted again only once no kept retirement names it ([`RegionalConfiguration::admits`]), so that
  /// takeover is done and every object this node owned belongs to a successor now: its entries naming itself
  /// are stale and are forgotten, and later lookups forward to the successors rather than serving a stale copy
  /// (§4.8 "Leases and reads", AUD-08; "Authority scope": the bumped epoch fences every object owned by that
  /// host). Forwarding an owned volume with no routing entry to its successor is the broader ledger-transfer
  /// contract (GAP-A9-7).
  pub fn install_configuration(&mut self, configuration: Configuration, members: &[HostId]) {
    if configuration.host_epoch.0 > self.configuration.host_epoch.0 {
      self.routing.forget_owned_by(self.host);
    }
    self.members = members.to_vec();
    self.configuration = configuration;
    let _ = self
      .acceptor
      .install_authority(Self::authority(&self.configuration));
  }

  /// The current configuration (the authority the async register drivers carry): the owner, the host
  /// epoch, the neighbourhood the candidates are drawn from, the quorum, and the version a request carries.
  /// Read per request; written only by [`install_configuration`](FleetNode::install_configuration) when the
  /// council commits a change.
  pub fn configuration(&self) -> &Configuration {
    &self.configuration
  }

  /// The regional members the last installed configuration named (the argument
  /// [`install_configuration`](FleetNode::install_configuration) last received). Paired with
  /// [`configuration`](FleetNode::configuration) when a node fans its committed configuration to its other
  /// shards, so each installs the same `(configuration, members)` the council committed.
  pub fn members(&self) -> &[HostId] {
    &self.members
  }

  /// The SWIM membership view — the alive set the neighbourhood tracks, and the state the live probe
  /// loop (owed) reads and gossips.
  pub fn membership(&self) -> &Membership {
    &self.membership
  }

  /// Records that `owner`, the author of a record this node accepted under `regional` (§4.8), owns
  /// `object`. A non-member owner is refused typed ([`RegisterError::Unauthorized`]): only a member writes.
  pub fn track_object(
    &mut self,
    object: ObjectId,
    owner: HostId,
    regional: &RegionalConfiguration,
  ) -> Result<(), RegisterError> {
    if !regional.members.contains(&owner) {
      return Err(RegisterError::Unauthorized);
    }
    self.routing.track(object, owner);
    Ok(())
  }

  /// Moves `object` to `owner`: the successor a retirement names for it, or this node once it adopted it.
  pub fn track_object_owner(&mut self, object: ObjectId, owner: HostId) {
    self.routing.track(object, owner);
  }

  /// Forgets `object` (destroyed, or no longer held here).
  pub fn forget_object(&mut self, object: ObjectId) {
    self.routing.forget(object);
  }

  /// The current owner of `object` as this node's routing sees it, or `None` if it holds no copy.
  pub fn object_owner(&self, object: ObjectId) -> Option<HostId> {
    self.routing.owner_of(object)
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

  /// Folds a gossiped membership `update` about `subject` into the SWIM view (§4.8 "membership fed by
  /// SWIM"), returning whether the view changed. The configuration is **not** reconciled here — the regional
  /// council is the authority (D-14, one configuration group per region): the council leader reconciles the
  /// region from this view ([`RegionalCouncil::reconcile_alive`](crate::config_group::RegionalCouncil::reconcile_alive))
  /// and every node installs the committed result ([`install_configuration`](FleetNode::install_configuration)),
  /// which is also where a departed owner's objects are taken over. So `observe` only advances the failure
  /// view the leader reconciles from; a non-leader's view still feeds the leader (SWIM disseminates it) and
  /// the leader's committed configuration comes back to it. At `f = 0` the sole-voter council is its own
  /// leader, so the view still drives the configuration through the same path (R8). It never removes this
  /// host — the owner is never retired.
  ///
  /// A member becoming alive — a join, a refutation of a suspicion or a death, a rejoin — is logged with the
  /// call site that folded it (`#[track_caller]` through the folds below): a handful per node's life, and the
  /// evidence a revival the design does not expect is traced by (GAPS 2026-09-29: retired peers' manifest seed
  /// ids held alive).
  #[track_caller]
  pub fn observe(&mut self, subject: HostId, update: MemberState) -> bool {
    let prior = self.membership.state(subject);
    let changed = self.membership.apply(subject, update).is_some();
    if changed
      && let Some(state) = self.membership.state(subject)
      && prior.is_none_or(|prior| prior.liveness != Liveness::Alive)
      && state.liveness == Liveness::Alive
    {
      eprintln!(
        "slates-cluster: fleet: {:?} holds {subject:?} alive: {prior:?} -> {state:?}, folded at {}",
        self.host,
        std::panic::Location::caller()
      );
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
  /// This borrows the configuration (`&self.configuration`) and the acceptor (`&mut self.acceptor`) —
  /// disjoint fields — so no clone is needed on the write path; the configuration is read, not copied, per
  /// commit. The caller supplies the connected candidate holders (the live probe/gossip loop that keeps
  /// them connected is owed) and the derived budget.
  pub async fn commit_head(
    &mut self,
    record: &Record,
    remote_holders: Vec<(HostId, Endpoint)>,
    budget: CommitBudget,
  ) -> Committed {
    commit_under_configuration(
      &self.configuration,
      record,
      &mut self.acceptor,
      remote_holders,
      budget,
    )
    .await
  }
}

/// Folds one peer's believed `state` into `fleet`'s membership: the membership plane's detector's belief, which the
/// control shard folds into its own `FleetNode` and hands every other shard to apply to theirs (every shard is an
/// owner with its own membership view, D-7), so all views advance identically and deterministically. A confirmed death and
/// an alive peer are both folded (idempotent); a suspect, or a peer not yet seen, leaves the view untouched
/// (a suspect is still a member until a confirmed death). Returns whether the membership changed — the
/// takeovers a death produces come from [`install_configuration`](FleetNode::install_configuration) once the
/// council commits the retirement.
#[track_caller]
pub fn apply_peer_state(fleet: &mut FleetNode, peer: HostId, state: Option<MemberState>) -> bool {
  if peer == fleet.host() {
    return false;
  }
  match state {
    Some(state) if state.liveness == Liveness::Dead || state.liveness == Liveness::Alive => {
      fleet.observe(peer, state)
    }
    _ => false,
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

  /// A test-only configuration for `owner` over `members` at `quorum`, each neighbourhood bounded to the
  /// candidate floor — the shape [`FleetNode::install_configuration`] receives from the council.
  fn config(owner: HostId, members: &[HostId], quorum: Quorum) -> Configuration {
    let scatter = u64::try_from(quorum.candidates()).unwrap_or(u64::MAX);
    RegionalConfiguration::formed(
      members.to_vec(),
      quorum,
      std::collections::BTreeMap::new(),
      scatter,
      false,
    )
    .configuration_for(owner)
    .unwrap_or_else(|| Configuration::solo(owner))
  }

  /// Installs a council configuration built from `members` at `quorum` into `node` (deriving its owner view
  /// and passing the region members) — the shape the daemon's council sync gives.
  fn install(node: &mut FleetNode, members: &[HostId], quorum: Quorum) {
    let configuration = config(node.host(), members, quorum);
    node.install_configuration(configuration, members);
  }

  /// Installs `regional`'s view for `node`'s owner (used where the version must advance through real
  /// admits/retires on the regional configuration).
  fn install_regional(node: &mut FleetNode, regional: &RegionalConfiguration) {
    let owner = node.host();
    let configuration = regional
      .configuration_for(owner)
      .unwrap_or_else(|| Configuration::solo(owner));
    node.install_configuration(configuration, &regional.members);
  }

  /// The invariants the owner runtime holds at *every* scale (checked identically at N=1 and in a fleet —
  /// the R8 differential): the owner is in its own neighbourhood, and its acceptor authorizes a head under
  /// the configuration's current version (its authority is in step with the installed configuration) but
  /// refuses one under a stale generation. The neighbourhood tracking the alive set is now the council's
  /// property (it reconciles the region from the membership and every node installs the result), proven in
  /// `config_group.rs`; here the runtime's own invariant is that it places and fences under whatever
  /// configuration it has installed.
  fn assert_runtime_invariants(node: &mut FleetNode) {
    let owner = node.configuration().owner;
    let version = node.configuration().version;
    let neighbourhood = node.configuration().neighbourhood.clone();

    assert!(
      neighbourhood.contains(&owner),
      "the owner is in its own neighbourhood"
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

  /// AC (§4.8, D-14): the runtime installs the configuration the council commits — a member the council
  /// admitted appears in the neighbourhood at the committed version, one it retired is gone, and the owner's
  /// acceptor authority stays in step across each install (checked by the invariants). The council decides
  /// the configuration (proven in `config_group.rs`); the runtime places and fences under what it installs.
  #[test]
  fn installing_a_configuration_updates_placement_and_keeps_authority_in_step() {
    let mut node = FleetNode::new(SELF, Quorum { f: 1 }, &[]);
    assert_runtime_invariants(&mut node);

    // The council reaches a configuration by admitting A then B (two committed changes advance the version).
    let mut regional = RegionalConfiguration::formed(
      vec![SELF],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    regional.admit(A, None, 3);
    regional.admit(B, None, 3);
    install_regional(&mut node, &regional);
    assert!(node.configuration().neighbourhood.contains(&A));
    assert!(node.configuration().neighbourhood.contains(&B));
    assert!(
      node.configuration().version > 0,
      "each committed admit advanced the version"
    );
    assert_runtime_invariants(&mut node);

    // The council retires A: install the new configuration. A is gone; the version advanced; the owner's
    // acceptor authority stays in step (the invariants check it authorizes the new version and refuses the old).
    regional.retire(A, 3);
    install_regional(&mut node, &regional);
    assert!(
      !node.configuration().neighbourhood.contains(&A),
      "the retired member is gone from the neighbourhood"
    );
    assert_runtime_invariants(&mut node);
  }

  /// `observe` advances only the SWIM view (the council leader reconciles the configuration from it); it
  /// does not itself change the configuration — a fresh member is seen alive, a death seen dead.
  #[test]
  fn observe_advances_the_membership_view() {
    let mut node = FleetNode::new(SELF, Quorum { f: 1 }, &[]);
    assert!(node.observe(A, alive(0)), "a fresh member changes the view");
    assert_eq!(
      node.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Alive)
    );
    assert!(
      !node.observe(A, alive(0)),
      "a stale re-assertion changes nothing"
    );
    assert!(node.observe(A, dead(1)), "a death changes the view");
    assert_eq!(
      node.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Dead)
    );
  }

  /// AC (R8, the named differential): the owner runtime has identical observable semantics at N=1 and
  /// in a simulated fleet. The **same** code, driven with no peers (`f = 0`) and with two peers
  /// (`f = 1`), holds the same invariants after the same shape of history — there is no mode switch,
  /// only a different N in one formula family.
  #[test]
  fn the_owner_runtime_has_identical_semantics_at_n1_and_in_a_fleet() {
    // N=1: the laptop. Its solo council's formed configuration is the region {SELF}; the invariants hold.
    let mut laptop = FleetNode::solo(SELF);
    assert_runtime_invariants(&mut laptop);
    assert_eq!(laptop.configuration().neighbourhood, vec![SELF]);

    // Fleet: the same runtime, installing the council's configurations. The invariants hold after each
    // install — the identical assertions the laptop passed, the same `install_configuration` code path.
    let mut fleet = FleetNode::new(SELF, Quorum { f: 1 }, &[A, B]);
    assert_runtime_invariants(&mut fleet);
    // The council retires A, then B (deaths committed): install each committed configuration.
    install(&mut fleet, &[SELF, B], Quorum { f: 1 });
    assert_runtime_invariants(&mut fleet);
    install(&mut fleet, &[SELF], Quorum { f: 1 });
    assert_runtime_invariants(&mut fleet);
    // Every peer gone, the fleet has degenerated to exactly the laptop's observable configuration: one
    // member, itself the owner — reached by the same code, no mode switch.
    assert_eq!(
      fleet.configuration().neighbourhood,
      vec![SELF],
      "with every peer retired the fleet configuration equals the laptop's"
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

  /// AC (§4.8 "Placement", D-14): a fleet strictly larger than the candidate floor bounds each owner's
  /// neighbourhood to exactly `2f+1` — one copyset, the lowest-loss placement — never the whole alive set.
  /// Unbounded placement scatters every object over all N nodes (the `Θ(S^f)` data-loss case the Copysets
  /// research forbids); the derived floor holds the copyset count linear. Non-vacuous: the neighbourhood is
  /// a strict subset of the five-node fleet, and every candidate an object lands on is drawn from it.
  #[test]
  fn a_fleet_wider_than_the_candidate_floor_bounds_the_neighbourhood_to_one_copyset() {
    // Five hosts at f=1: the candidate floor is 2f+1 = 3, so SELF's neighbourhood is itself plus two peers,
    // not all five (SELF, A, B exist; two more push the fleet past the floor).
    let d = HostId(4);
    let e = HostId(5);
    let peers = [A, B, d, e];
    let node = FleetNode::new(SELF, Quorum { f: 1 }, &peers);
    let hood = node.configuration().neighbourhood.clone();
    assert_eq!(
      hood.len(),
      3,
      "bounded to the candidate floor 2f+1 = 3, not the fleet size 5 (unbounded would scatter over all \
       five — the Θ(S^f) data-loss case)"
    );
    assert_eq!(hood[0], SELF, "the owner heads its own neighbourhood");

    // Deterministic: the same fleet builds the identical bounded neighbourhood.
    let again = FleetNode::new(SELF, Quorum { f: 1 }, &peers);
    assert_eq!(
      again.configuration().neighbourhood,
      hood,
      "the bound is deterministic — the same alive set yields the same neighbourhood"
    );

    // Every candidate an object lands on is drawn from the bounded neighbourhood — an object is confined to
    // the owner's one copyset, never scattered across the wider fleet.
    for i in 0..32u64 {
      let placed = node.configuration().place(ObjectId::new(SELF, i));
      for holder in &placed.candidates {
        assert!(
          hood.contains(holder),
          "a candidate holder must come from the bounded neighbourhood, not the wider fleet"
        );
      }
    }
  }

  /// §4.8 "Promotion and takeover": this node's own epoch advancing means the council took it over while it was
  /// away and has since re-admitted it — which it does only once that takeover is done — so every entry naming
  /// itself the owner is stale and forgotten, while the peers' objects it backs stay.
  #[test]
  fn a_re_admitted_node_forgets_the_objects_it_owned() {
    let quorum = Quorum { f: 1 };
    let mut regional = RegionalConfiguration::formed(
      vec![SELF, A, B],
      quorum,
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    let mut node = FleetNode::new(SELF, quorum, &[A, B]);
    install_regional(&mut node, &regional);
    let own = ObjectId::new(SELF, 1);
    let backed = ObjectId::new(A, 1);
    node.track_object_owner(own, SELF);
    node.track_object(backed, A, &regional).unwrap();
    regional.take_over(SELF, 3);
    for survivor in regional.survivors_of(SELF) {
      regional.confirm(SELF, survivor);
    }
    assert!(
      regional.admit(SELF, None, 3),
      "re-admitted once its takeover is done"
    );
    install_regional(&mut node, &regional);
    assert_eq!(
      node.object_owner(own),
      None,
      "its own object belongs to a successor now"
    );
    assert_eq!(node.object_owner(backed), Some(A), "a backed object stays");
  }

  /// §4.8: only a member writes, so a record's owner that is not a member is never tracked.
  #[test]
  fn tracking_refuses_an_owner_that_is_not_a_member() {
    let quorum = Quorum { f: 1 };
    let regional = RegionalConfiguration::formed(
      vec![SELF, A],
      quorum,
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    let mut node = FleetNode::new(SELF, quorum, &[A]);
    let object = ObjectId::new(B, 1);
    assert_eq!(
      node.track_object(object, B, &regional),
      Err(RegisterError::Unauthorized)
    );
    assert_eq!(node.object_owner(object), None);
  }

  /// AC (§4.8, the membership bridge): `apply_peer_state` folds the detector's belief about a peer into the owner
  /// runtime's membership, the failure view the council leader reconciles the configuration from. A death is folded
  /// dead and an alive peer alive; a suspicion leaves the member as it was (still a member until confirmed); a stale
  /// alive belief at a lower incarnation cannot resurrect a death; a repeat is a no-op.
  #[test]
  fn apply_peer_state_folds_deaths_and_joins_and_leaves_suspicions() {
    let mut fleet = FleetNode::new(SELF, Quorum { f: 1 }, &[A]);
    assert!(apply_peer_state(&mut fleet, B, Some(alive(0))), "B joins");
    assert!(apply_peer_state(&mut fleet, A, Some(dead(1))), "A dies");
    assert_eq!(
      fleet.membership().state(A).map(|s| s.liveness),
      Some(Liveness::Dead)
    );
    assert!(
      !apply_peer_state(&mut fleet, A, Some(alive(0))),
      "stale alive gossip cannot resurrect A"
    );
    assert!(
      !apply_peer_state(
        &mut fleet,
        B,
        Some(MemberState {
          liveness: Liveness::Suspect,
          incarnation: 0,
        })
      ),
      "a suspicion is not folded"
    );
    assert_eq!(
      fleet.membership().state(B).map(|s| s.liveness),
      Some(Liveness::Alive)
    );
    assert!(
      !apply_peer_state(&mut fleet, A, Some(dead(1))),
      "a repeat is a no-op"
    );
    assert!(
      !apply_peer_state(&mut fleet, SELF, Some(dead(9))),
      "this node is never folded"
    );
  }
}
