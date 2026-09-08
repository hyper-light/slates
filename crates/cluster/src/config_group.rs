//! The configuration group (§4.8 "Configuration, by consensus", D-14) — the authority that maintains
//! the versioned [`Configuration`] every register request carries: membership, the neighbourhood
//! candidates are drawn from, the fault tolerance, and the host epochs. It is touched **only** on
//! membership, takeover, neighbourhood and home changes — never on a per-write path (banned item 10) —
//! and read per request from a local copy.
//!
//! Degenerate on a laptop (`f = 0`): one voter, the local node, whose configuration advances by a
//! local append. In a fleet (`f > 0`) the regional group agrees each configuration change by consensus
//! (the hecate Raft dialect — leader election and log replication among the voters); that consensus is
//! **owed**, and this slice is its `f = 0` degenerate plus the bridge from the SWIM membership view: a
//! reconfiguration is proposed when a member joins or dies, and — at `f = 0` — applied locally,
//! bumping the version so a request under the stale version is refused (`ConfigurationStale`). The same
//! interface will carry a fleet proposal through consensus, changing no caller (R8).
//!
//! Built here: the neighbourhood reconcile (above) and **takeover** ([`ConfigGroup::take_over`]) — when
//! SWIM declares the owner dead, the group reassigns the volume to the rendezvous-first survivor, bumps
//! the host epoch, and advances the generation, so a resumed stale owner is fenced (by the advanced
//! configuration generation in this single-generation model — `ConfigurationStale`/`ForeignGeneration`).
//! Owed with the consensus: the per-host epoch fence of the design's `FencedRegister` (A-9), the new
//! owner's phase-one recovery (reading the dead owner's highest records from the holders and adopting
//! the head before serving), the home/mirror moves, and the fenced replication of the configuration log
//! across the voters.

use slates_db::register::{Configuration, HostEpoch, HostId, rendezvous_first};

use crate::membership::Membership;

/// A proposed change to the configuration — the vocabulary the SWIM view and takeover speak to the
/// group. Applied locally at `f = 0`; carried through consensus at `f > 0` (owed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconfiguration {
  /// Admit a member to the neighbourhood (a join the membership view learned).
  Admit(HostId),
  /// Retire a member from the neighbourhood (a death the membership view confirmed).
  Retire(HostId),
}

/// A refusal to take over a dead owner's volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverError {
  /// The host being taken over is not the volume's current owner — a non-owner death is a neighbourhood
  /// [`Retire`](Reconfiguration::Retire), not a takeover.
  NotOwner {
    /// The current owner (the one a takeover would have to name).
    owner: HostId,
  },
  /// No survivor remains in the neighbourhood to take the volume — every candidate is gone. The volume
  /// is unrecoverable from this configuration (the correlated-loss case the neighbourhood bound makes
  /// rare); the caller reports the loss rather than inventing an owner.
  NoSurvivor,
}

/// The configuration group on one node: the current [`Configuration`] it serves. At `f = 0` it is the
/// sole voter and advances the configuration by a local append; the fleet consensus that agrees each
/// change among `f + 1` voters is owed.
pub struct ConfigGroup {
  configuration: Configuration,
}

impl ConfigGroup {
  /// The solo configuration group — one voter, `owner`, `f = 0`, version zero (the laptop degenerate).
  pub fn solo(owner: HostId) -> ConfigGroup {
    ConfigGroup {
      configuration: Configuration::solo(owner),
    }
  }

  /// A group over an existing configuration (a fleet member reading the group's current configuration).
  pub fn from_configuration(configuration: Configuration) -> ConfigGroup {
    ConfigGroup { configuration }
  }

  /// The current configuration — read per request; a request carrying an older version is refused with
  /// this one ([`Configuration::check_version`]).
  pub fn configuration(&self) -> &Configuration {
    &self.configuration
  }

  /// Proposes a reconfiguration and, at `f = 0`, applies it locally — admitting or retiring a member
  /// in the neighbourhood — bumping the version when the neighbourhood actually changes, so a request
  /// under the stale version is refused. Returns whether the configuration changed. (At `f > 0` this
  /// proposal is agreed by consensus first; owed.)
  pub fn reconfigure(&mut self, change: Reconfiguration) -> bool {
    let changed = match change {
      Reconfiguration::Admit(host) => {
        if self.configuration.neighbourhood.contains(&host) {
          false
        } else {
          self.configuration.neighbourhood.push(host);
          self
            .configuration
            .neighbourhood
            .sort_unstable_by_key(|h| h.0);
          true
        }
      }
      Reconfiguration::Retire(host) => {
        // The owner is never retired from its own configuration.
        if host == self.configuration.owner || !self.configuration.neighbourhood.contains(&host) {
          false
        } else {
          self.configuration.neighbourhood.retain(|h| *h != host);
          true
        }
      }
    };
    if changed {
      self.configuration.version = self.configuration.version.saturating_add(1);
    }
    changed
  }

  /// Reconciles the configuration's neighbourhood with a SWIM membership `view`: admits every alive
  /// member not yet in the neighbourhood and retires every neighbourhood member no longer alive — the
  /// bridge from failure detection to the configuration authority. Returns whether the configuration
  /// changed (the version having advanced once per change). At `f > 0` each admit/retire is a consensus
  /// proposal (owed).
  pub fn reconcile(&mut self, view: &Membership) -> bool {
    let alive = view.alive();
    let mut changed = false;
    for &host in &alive {
      changed |= self.reconfigure(Reconfiguration::Admit(host));
    }
    let stale: Vec<HostId> = self
      .configuration
      .neighbourhood
      .iter()
      .copied()
      .filter(|host| !alive.contains(host))
      .collect();
    for host in stale {
      changed |= self.reconfigure(Reconfiguration::Retire(host));
    }
    changed
  }

  /// Takes over the volume from a dead owner (§4.8 "Promotion and takeover"): SWIM has declared the
  /// current owner `dead`, so the group assigns the volume to the survivor of the neighbourhood that
  /// rendezvous ranks first for `object`, **bumps the host epoch** (the successor serves under the new
  /// epoch), drops the dead owner from the neighbourhood, and advances the version. Returns the new
  /// configuration, or a [`TakeoverError`] if the named host is not the owner or no survivor remains.
  ///
  /// In this single-generation model a resumed stale owner is fenced by the advanced generation: it
  /// still holds the old configuration, so its request is refused `ConfigurationStale`, and a record it
  /// ships to a holder now serving the new generation is refused `ForeignGeneration`/`Unauthorized`. The
  /// design's per-host epoch fence — every holder raising its fence *for the dead host* to the new epoch,
  /// so the zombie is refused `StaleEpoch{new}` even under its own owner id — is the `FencedRegister`
  /// per-host model (A-9), owed. The new owner also still owes the phase-one recovery (reading the dead
  /// owner's highest records from the holders and adopting the head) before it serves. At `f > 0` the
  /// takeover decision is agreed by the configuration consensus (owed); this is its local effect.
  pub fn take_over(&mut self, dead: HostId, object: u64) -> Result<&Configuration, TakeoverError> {
    if dead != self.configuration.owner {
      return Err(TakeoverError::NotOwner {
        owner: self.configuration.owner,
      });
    }
    let survivors: Vec<HostId> = self
      .configuration
      .neighbourhood
      .iter()
      .copied()
      .filter(|host| *host != dead)
      .collect();
    let successor = rendezvous_first(&survivors, object).ok_or(TakeoverError::NoSurvivor)?;

    self.configuration.owner = successor;
    // Bump the authority so the dead owner's in-flight records are fenced at the holders (the next
    // epoch, never a tunable — a monotonic step like the version).
    self.configuration.host_epoch = HostEpoch(self.configuration.host_epoch.0.saturating_add(1));
    self.configuration.neighbourhood = survivors;
    self.configuration.version = self.configuration.version.saturating_add(1);
    Ok(&self.configuration)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::membership::{Liveness, MemberState};

  const OWNER: HostId = HostId(1);
  const A: HostId = HostId(2);
  const B: HostId = HostId(3);
  const C: HostId = HostId(4);

  /// Admitting a member grows the neighbourhood and advances the version; a duplicate admit is a no-op.
  #[test]
  fn reconfigure_admits_a_member_advancing_the_version() {
    let mut group = ConfigGroup::solo(OWNER);
    assert_eq!(group.configuration().version, 0);
    assert_eq!(group.configuration().neighbourhood, vec![OWNER]);

    assert!(group.reconfigure(Reconfiguration::Admit(A)));
    assert_eq!(group.configuration().neighbourhood, vec![OWNER, A]);
    assert_eq!(
      group.configuration().version,
      1,
      "the version advanced on the change"
    );

    assert!(
      !group.reconfigure(Reconfiguration::Admit(A)),
      "a duplicate admit is a no-op"
    );
    assert_eq!(group.configuration().version, 1);
  }

  /// Retiring a member shrinks the neighbourhood and advances the version; the owner is never retired.
  #[test]
  fn reconfigure_retires_a_member_and_never_the_owner() {
    let mut group = ConfigGroup::solo(OWNER);
    group.reconfigure(Reconfiguration::Admit(A));

    assert!(group.reconfigure(Reconfiguration::Retire(A)));
    assert_eq!(group.configuration().neighbourhood, vec![OWNER]);
    assert_eq!(group.configuration().version, 2);

    assert!(
      !group.reconfigure(Reconfiguration::Retire(OWNER)),
      "the owner is never retired"
    );
    assert_eq!(group.configuration().version, 2);
  }

  /// Reconciling with a membership view admits alive peers and retires the dead — the SWIM-to-config
  /// bridge — advancing the version, and is a no-op when the view already matches.
  #[test]
  fn reconcile_tracks_the_membership_view() {
    let mut group = ConfigGroup::solo(OWNER);
    let mut view = Membership::new(OWNER);
    view.apply(
      A,
      MemberState {
        liveness: Liveness::Alive,
        incarnation: 0,
      },
    );
    view.apply(
      B,
      MemberState {
        liveness: Liveness::Alive,
        incarnation: 0,
      },
    );

    assert!(group.reconcile(&view), "alive peers are admitted");
    assert_eq!(group.configuration().neighbourhood, vec![OWNER, A, B]);
    let after_admit = group.configuration().version;
    assert!(after_admit >= 2, "the version advanced once per admit");

    assert!(
      !group.reconcile(&view),
      "reconciling an unchanged view changes nothing"
    );
    assert_eq!(group.configuration().version, after_admit);

    // B dies; reconciling retires it from the neighbourhood.
    view.apply(
      B,
      MemberState {
        liveness: Liveness::Dead,
        incarnation: 0,
      },
    );
    assert!(group.reconcile(&view), "a dead peer is retired");
    assert_eq!(group.configuration().neighbourhood, vec![OWNER, A]);
    assert!(group.configuration().version > after_admit);
  }

  /// Taking over a dead owner reassigns the volume to the rendezvous-first survivor of the
  /// neighbourhood, bumps the host epoch, drops the dead owner, and advances the generation.
  #[test]
  fn take_over_reassigns_to_the_rendezvous_first_survivor_and_bumps_the_epoch() {
    let mut group = ConfigGroup::solo(OWNER);
    group.reconfigure(Reconfiguration::Admit(A));
    group.reconfigure(Reconfiguration::Admit(B));
    group.reconfigure(Reconfiguration::Admit(C));
    assert_eq!(group.configuration().host_epoch, HostEpoch(1));
    let before = group.configuration().version;

    let object = 42;
    let new = group
      .take_over(OWNER, object)
      .expect("a survivor takes over")
      .clone();

    let survivors = [A, B, C];
    assert!(survivors.contains(&new.owner), "a survivor took over");
    assert_eq!(
      new.owner,
      rendezvous_first(&survivors, object).unwrap(),
      "the rendezvous-first survivor is chosen"
    );
    assert_eq!(new.host_epoch, HostEpoch(2), "the host epoch is bumped");
    assert!(
      !new.neighbourhood.contains(&OWNER),
      "the dead owner left the neighbourhood"
    );
    assert!(new.version > before, "the generation advanced");
  }

  /// Taking over a host that is not the current owner is refused — a non-owner death is a neighbourhood
  /// retire, not a takeover.
  #[test]
  fn take_over_of_a_non_owner_refuses() {
    let mut group = ConfigGroup::solo(OWNER);
    group.reconfigure(Reconfiguration::Admit(A));
    assert_eq!(
      group.take_over(A, 42),
      Err(TakeoverError::NotOwner { owner: OWNER }),
      "only the current owner is taken over"
    );
  }

  /// Taking over when no survivor remains is refused — the volume is unrecoverable from this
  /// configuration rather than assigned to a phantom owner.
  #[test]
  fn take_over_with_no_survivor_refuses() {
    let mut group = ConfigGroup::solo(OWNER);
    assert_eq!(
      group.take_over(OWNER, 42),
      Err(TakeoverError::NoSurvivor),
      "a lone owner has no survivor to take over"
    );
  }
}
