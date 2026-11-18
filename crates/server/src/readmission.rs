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
//! it, so a boot's install never yields. A yielded volume stays yielded: a later takeover that names this node its
//! successor again finds the id present and keeps the stale copy (`verbs::materialize_taken_over`), so serving it
//! would need that rebuild first, which is owed with the release of a yielded copy's memory (the ledger transfer
//! GAP-A9-7 owes).

use std::collections::BTreeSet;

use slates_db::HostId;
use slates_db::catalog::VolumeId;
use slates_db::register::Configuration;

use crate::state::ShardState;

/// Format: the status count of the volumes a shard yielded at its host's re-admission.
pub(crate) const YIELDED: &str = "fleet.readmission.yielded";

/// Installs `configuration`, the council's committed placement for this node, on this shard's fleet node; when it
/// carries a newer epoch for the same host than the configuration installed before it, the host was taken over while
/// away and admitted again, and every volume this shard catalogues is yielded ([`yield_catalogued`]).
pub(crate) fn install_placement(
  state: &mut ShardState,
  configuration: Configuration,
  members: &[HostId],
) {
  let installed = state.fleet.configuration();
  let readmitted =
    configuration.owner == installed.owner && configuration.host_epoch.0 > installed.host_epoch.0;
  state.fleet.install_configuration(configuration, members);
  if readmitted {
    yield_catalogued(state);
  }
}

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
  #[cfg(target_os = "linux")]
  for volume in newly {
    crate::fuse::superseded(state, volume);
  }
}
