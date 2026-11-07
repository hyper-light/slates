//! A clone's name held at the name's partition (§4.8 "Lookup", A-236): the pure decisions of the protocol that makes
//! it, and of the recovery that reconciles it.
//!
//! **What it answers.** A name routes by its hash to one partition, where it is looked up and where its uniqueness is
//! decided (`owner_of_name`). A clone must live beside its origin, for copy-on-write, so a clone whose name hashes
//! elsewhere was unreachable by name and its name could be created a second time
//! (`docs/bugs/2026-09-13-clone-name-not-routable-by-hash.md`). The name's partition now holds an alias pointing at the
//! clone. A lookup still routes by hash, and the name's partition forwards it to the clone's. A second create of the
//! name finds the alias and is refused by kind. Rejected: refusing a clone name that hashes elsewhere (a rule a user
//! cannot predict) and scattering a lookup over every partition on a miss (against D-14's "ids route to owners").
//!
//! **The protocol** ([`CLONE_STEPS`]): the clone's two partitions are written in order, each step one durable
//! operation. The alias is reserved at the name's partition, the clone made at its origin's, and the alias confirmed.
//! A reserved alias holds the name (a create of it is refused, `AlreadyExists`) but resolves nothing ([`resolve`]): a
//! lookup of it is refused by kind until it is confirmed. A clone that cannot be made releases its reservation. A
//! destroyed clone releases its alias after the destroy ([`DESTROY_STEPS`]).
//!
//! **Recovery.** A crash can fall between any two steps. A recovered daemon reconciles each alias against the volume
//! its id names on the clone's partition ([`reconcile`]):
//! - a reserved alias whose clone exists under its name is confirmed;
//! - an alias whose clone does not exist, or no longer holds the name, is released.
//!
//! So after any crash and its recovery the name either belongs to no one or resolves to the clone that holds it, which
//! `crates/db/tests/alias.rs` checks after a crash at every step. Reconciling only at recovery is what makes it safe: a
//! running clone sits between its steps on purpose, and a reconcile then would release its reservation.
//!
//! With one partition every name hashes to the clone's own partition, so no alias is ever made: the same code with one
//! partition (R8), held by the N=1 differential in the server's tests.

use crate::catalog::{NameAliasRecord, VolumeId, VolumeRecord};

/// One durable step of the protocol, each on one partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
  /// Reserve the alias at the name's partition.
  Reserve,
  /// Make the clone at its origin's partition.
  Create,
  /// Confirm the alias at the name's partition.
  Confirm,
  /// Destroy the clone at its partition.
  Destroy,
  /// Release the alias at the name's partition.
  Release,
}

/// The steps that make a clone whose name hashes to another partition, in order.
pub const CLONE_STEPS: [Step; 3] = [Step::Reserve, Step::Create, Step::Confirm];

/// The steps that destroy such a clone, in order.
pub const DESTROY_STEPS: [Step; 2] = [Step::Destroy, Step::Release];

/// What a recovery does with an alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reconciled {
  /// The alias stands as it is.
  Keep,
  /// The clone was made under the name: the reservation is confirmed.
  Confirm,
  /// No clone holds the name: the alias is released, and the name with it.
  Release,
}

/// The reconciliation of `alias` against `volume`, the record its id names on the clone's partition (none when that
/// partition holds no such volume).
pub fn reconcile(alias: &NameAliasRecord, volume: Option<&VolumeRecord>) -> Reconciled {
  let holds = volume.is_some_and(|volume| volume.id == alias.volume && volume.name == alias.name);
  match (holds, alias.confirmed) {
    (true, true) => Reconciled::Keep,
    (true, false) => Reconciled::Confirm,
    (false, _) => Reconciled::Release,
  }
}

/// The volume a name's alias resolves to: the clone, once confirmed; nothing while reserved.
pub fn resolve(alias: &NameAliasRecord) -> Option<VolumeId> {
  alias.confirmed.then_some(alias.volume)
}
