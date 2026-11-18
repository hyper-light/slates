//! A host admitted again under its own id yields the volumes it catalogued (A-334; §4.8 "Authority scope", "Leases and
//! reads"; AUD-08).
//!
//! **What happens to such a host.** The council retires a host it judges dead. If the host was alive all along (a
//! stopped process resumed, a partition healed), it refutes its death and is admitted again under the same member id,
//! but only once no kept retirement names it ([`slates_db::register::RegionalConfiguration::admits`]): the takeover of
//! its objects is done by then, and its host epoch has advanced. Every object it owned belongs to a successor, whose
//! writes have passed the host's own copy. The design's rule is that "later lookups forward to the successors rather
//! than serving a stale copy" ([`slates_cluster::fleet::FleetNode::install_configuration`], which forgets the host's own
//! routing entries at the epoch's advance).
//!
//! **What went wrong.** The shards' catalogs kept the copies, and two things served them. A client's verb on a volume
//! this node created or catalogues runs here (`verbs::owned_by_another_host`). And the owner lease is the host's, not
//! the object's, so it confirms again once the holders answer the re-admitted host's probes. A read through such a host
//! answered from its stale copy: in the SIGSTOP owner flow of `crates/cli/tests/live_chaos.rs` the old owner, read the
//! moment it resumed, answered five of its successor's acknowledged files `NotFound` (goal g229, 2026-11-18). Its
//! status then showed `fleet_host_epoch` 2, the volume still catalogued with its epoch-1 shipper, and the read parked
//! for the lease and then served (`lease.wait.parked` 1, `lease.wait.served` 1).
//!
//! **The rule.** When a configuration a shard installs at run time carries a newer epoch for this same host, the shard
//! marks every volume it catalogues as yielded (`ShardState::yielded`). A client's verb on a yielded volume is forwarded
//! to its successor through the location round. Its latest state is refused here whatever the lease says
//! ([`crate::lease::LeaseVerdict::LostToSuccessor`]), so a mount or a peer's forward that still names this node never reads the
//! stale copy. A host's first epoch is [`slates_db::register::FIRST_EPOCH`], and a restarted daemon is a new member at
//! it, so a boot's install never yields. A yielded volume stays yielded until a later takeover names this node its
//! successor again: that takeover relinquishes the stale copy (its record with no tombstone, its slot torn down in
//! slices, its fence forgotten, the yield lifted) and makes the volume from the state it adopted (A-362,
//! `verbs::materialize_taken_over`). Until then the copy's memory stays held (released with GAP-A9-7's ledger
//! transfer).
//!
//! **Its live logs (A-409).** A yielded volume's live log ships nothing more and does no period work, as a fenced one
//! does (A-349), and every verb waiting on its durability is answered `DurabilityPending` at the yield
//! (`live_ship::yielded`): its node knows the loss before any holder can tell it, and a holder that refused the log as
//! not the volume's acting owner had never fenced it, so it shipped on, an install of its whole volume 1,714 times in
//! one run.
//!
//! **The holder's side (A-382).** The host also held register copies, as a candidate, of the objects it took over
//! before it was retired, and they still named it as their owner. At the same install it lets them go
//! ([`let_go_own_registers`]), so the successor's next record makes it a holder again, as it makes any new candidate
//! one, and the volume's live log follows; and it ships no record of a volume it lost (`fleet::unplaced_heads`), so no
//! stale owner's record can take a copy made afresh. Kept, they refused every record of their successors for good, and
//! the successor never shipped its live log to a holder that did not hold its catalog: in the chaos nemesis the
//! superseded adopter's copy stayed behind the creator's log at fence 3 for a whole fleet wait (seeds
//! 6876545201569229162 and 7236833171758868842, CI diag 44345243646).

use std::collections::BTreeSet;

use slates_db::HostId;
use slates_db::catalog::VolumeId;
use slates_db::register::Configuration;

use crate::state::ShardState;

/// Format: the status count of the volumes a shard yielded at its host's re-admission.
pub(crate) const YIELDED: &str = "fleet.readmission.yielded";

/// Format: the status count of the register copies a shard let go at its host's re-admission because they named the
/// host itself as the owner ([`let_go_own_registers`], A-382).
pub(crate) const REGISTERS_LET_GO: &str = "fleet.readmission.registers_let_go";

/// Installs `configuration`, the council's committed placement for this node, on this shard's fleet node; when it
/// carries a newer epoch for the same host than the configuration installed before it, the host was taken over while
/// away and admitted again: every volume this shard catalogues is yielded ([`yield_catalogued`]), and every register
/// copy it holds under its own authority is let go ([`let_go_own_registers`]).
pub(crate) fn install_placement(
  state: &mut ShardState,
  configuration: Configuration,
  members: &[HostId],
) {
  let installed = state.fleet.configuration();
  let local = installed.owner;
  let readmitted =
    configuration.owner == installed.owner && configuration.host_epoch.0 > installed.host_epoch.0;
  state.fleet.install_configuration(configuration, members);
  if readmitted {
    yield_catalogued(state);
    let_go_own_registers(state, local);
  }
}

/// Lets go every register copy this shard holds whose authority names this host (A-382): the copies it kept, as a
/// candidate holder, of the objects it took over before it was retired. Its takeover completed before it could be
/// admitted again ([`slates_db::register::RegionalConfiguration::admits`]), so each such object is its successor's,
/// and the successor adopted every position of it that ever committed. Nothing else would ever move these copies: the
/// re-admission forgot the host's own routes ([`slates_cluster::fleet::FleetNode::install_configuration`]), and the
/// install moves a copy's authority only to the owner its route names (`fleet::reconcile_held_authority`), so each kept
/// naming this host at its old generation and refused every record its successor shipped `ForeignGeneration`. The
/// successor ships a live log only to a holder that holds its volume's catalog (`live_ship::pending_sends`), so the
/// copy it kept here never caught up (the chaos nemesis, seeds 6876545201569229162 and 7236833171758868842, CI diag
/// 44345243646: 660 to 796 such refusals a run). Let go, the copy is made afresh by the successor's next record, as
/// a new candidate's is (`fleet::check_held_record`), and its live log follows. Only the control shard holds register
/// copies; on every other shard there is nothing to let go. The content held for those objects stays: a successor's
/// adopted head may still name this node among its holders (A-335).
fn let_go_own_registers(state: &mut ShardState, local: HostId) {
  let own: Vec<slates_db::register::ObjectId> = state
    .holder_records
    .iter()
    .filter(|(_, acceptor)| acceptor.owner() == local)
    .map(|(object, _)| *object)
    .collect();
  for object in &own {
    state.holder_records.remove(object);
  }
  state.count(
    REGISTERS_LET_GO,
    u64::try_from(own.len()).unwrap_or(u64::MAX),
  );
  // The let-go is kept before anything else is said of those copies (A-415): a warm restart restores a shard's held
  // copies from its last committed image and routes each to the owner it names, so a let-go no publication carried
  // came back naming this host, and the copies refused their successors again until the next re-admission. A refused
  // publication leaves the removals marked changed (`crate::tracked`), so the next one carries them.
  if !own.is_empty() && crate::verbs::publish_shard(state).is_err() {
    state.count(REGISTERS_LET_GO_UNPUBLISHED, 1);
  }
}

/// Format: the status count of re-admissions whose let-go of the host's own register copies the shard could not publish
/// at once ([`let_go_own_registers`], A-415); the next publication carries it.
pub(crate) const REGISTERS_LET_GO_UNPUBLISHED: &str =
  "fleet.readmission.registers_let_go_unpublished";

/// Marks every volume this shard catalogues as yielded, after pruning the set to the catalog (so it stays within the
/// partition's volume cap), counts the volumes it adds, and ends each newly yielded volume's FUSE mounts here at once
/// (`crate::fuse::superseded`, A-346), as a learned fence does: an idle mount sends no request that would find the yield,
/// so without the word it would keep its kernel's cached view of the stale copy. An NFS mount answers `NFS3ERR_STALE` at
/// its next request (`crate::nfs`'s gate reads `verbs::lost_to_a_successor`).
fn yield_catalogued(state: &mut ShardState) {
  let catalogued: BTreeSet<VolumeId> = state
    .db
    .partition()
    .volumes()
    .iter()
    .map(|record| record.id)
    .collect();
  state.yielded.retain(|volume| catalogued.contains(volume));
  let newly: Vec<VolumeId> = catalogued
    .into_iter()
    .filter(|volume| !state.yielded.contains(volume))
    .collect();
  state.yielded.extend(newly.iter().copied());
  state.count(YIELDED, u64::try_from(newly.len()).unwrap_or(u64::MAX));
  for volume in &newly {
    // Its live log ships nothing more and its writers are answered now (A-409).
    crate::live_ship::yielded(state, *volume);
  }
  #[cfg(target_os = "linux")]
  for volume in newly {
    crate::fuse::superseded(state, volume);
  }
}
