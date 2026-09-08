//! The configuration group (§4.8 "Configuration, by consensus", D-14) — the authority that maintains
//! the versioned [`Configuration`] every register request carries: membership, the neighbourhood
//! candidates are drawn from, the fault tolerance, and the host epochs. It is touched **only** on
//! membership, takeover, neighbourhood and home changes — never on a per-write path (banned item 10) —
//! and read per request from a local copy.
//!
//! The configuration is the deterministic fold of a **committed Raft log** ([`crate::raft`], the hecate
//! dialect): each change — admit, retire, takeover — is a [`ConfigCommand`] proposed to the log by the
//! leader and applied to the configuration only once committed, so every voter reaches the same
//! configuration, and the version bumps once per applied change so a request under the stale version is
//! refused (`ConfigurationStale`). Degenerate on a laptop (`f = 0`): the sole voter self-elects and its
//! append commits at once, so a change applies synchronously — the identical code path a fleet runs
//! through replication, never a mode switch (R8). The voter set is the config group's own membership
//! (fixed here); it is distinct from the volume neighbourhood the admit/retire commands grow.
//!
//! Built here: the change-through-the-log consensus (above), the SWIM-view [`reconcile`](ConfigGroup::reconcile)
//! bridge, and **takeover** ([`take_over`](ConfigGroup::take_over)) — when SWIM declares the owner dead,
//! the group reassigns the volume to the rendezvous-first survivor, bumps the host epoch, and advances
//! the generation, so a resumed stale owner is fenced (by the advanced configuration generation in this
//! single-generation model — `ConfigurationStale`/`ForeignGeneration`). Owed: driving the Raft live over
//! timers and the transport (this slice drives it sans-io), joint consensus to change the voter set, the
//! per-host epoch fence of the design's `FencedRegister` (A-9), and the new owner's phase-one recovery
//! (reading the dead owner's highest records from the holders and adopting the head before serving).

use std::mem::size_of;

use slates_db::register::{Configuration, HostEpoch, HostId, rendezvous_first};

use crate::membership::Membership;
use crate::raft::RaftNode;

/// A configuration change as it rides the Raft log — the command a committed [`LogEntry`](crate::raft::LogEntry)
/// carries, decoded and applied to the [`Configuration`] in commit order so every voter reaches the same
/// configuration. The Raft core treats it as opaque bytes; this is the config group's interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigCommand {
  /// Admit a member to the neighbourhood.
  Admit(HostId),
  /// Retire a member from the neighbourhood.
  Retire(HostId),
  /// Take over a dead owner's volume, reassigning `object` to the rendezvous-first survivor.
  TakeOver {
    /// The dead owner being taken over.
    dead: HostId,
    /// The volume object whose ownership moves.
    object: u64,
  },
}

/// Format: a config command is a one-byte tag followed by its little-endian fields; these are the tags.
const COMMAND_ADMIT: u8 = 0;
const COMMAND_RETIRE: u8 = 1;
const COMMAND_TAKE_OVER: u8 = 2;

impl ConfigCommand {
  /// The command's canonical bytes for the log: the tag, then the host id (and, for a takeover, the
  /// object), little-endian.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      ConfigCommand::Admit(host) => {
        out.push(COMMAND_ADMIT);
        out.extend_from_slice(&host.0.to_le_bytes());
      }
      ConfigCommand::Retire(host) => {
        out.push(COMMAND_RETIRE);
        out.extend_from_slice(&host.0.to_le_bytes());
      }
      ConfigCommand::TakeOver { dead, object } => {
        out.push(COMMAND_TAKE_OVER);
        out.extend_from_slice(&dead.0.to_le_bytes());
        out.extend_from_slice(&object.to_le_bytes());
      }
    }
    out
  }

  /// Decodes a command from a committed log entry, or `None` if the bytes are malformed (a corrupt log
  /// entry — never expected from our own [`encode`](ConfigCommand::encode), applied as a no-op if seen).
  pub fn decode(bytes: &[u8]) -> Option<ConfigCommand> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
      COMMAND_ADMIT => Some(ConfigCommand::Admit(take_host(rest)?.0)),
      COMMAND_RETIRE => Some(ConfigCommand::Retire(take_host(rest)?.0)),
      COMMAND_TAKE_OVER => {
        let (dead, rest) = take_host(rest)?;
        if rest.len() != size_of::<u64>() {
          return None;
        }
        let mut word = [0u8; size_of::<u64>()];
        word.copy_from_slice(rest);
        Some(ConfigCommand::TakeOver {
          dead,
          object: u64::from_le_bytes(word),
        })
      }
      _ => None,
    }
  }
}

/// Reads a u64 host id at the front of `bytes`, returning it and the remainder, or `None` if truncated.
fn take_host(bytes: &[u8]) -> Option<(HostId, &[u8])> {
  if bytes.len() < size_of::<u64>() {
    return None;
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Some((HostId(u64::from_le_bytes(word)), rest))
}

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

/// The configuration group on one node: the [`RaftNode`] over the configuration-group voters (the
/// hecate Raft dialect, §4.8 mechanism 2) and the [`Configuration`] built by applying its committed log
/// in order. A change — admit, retire, takeover — is a command proposed to the log and applied to the
/// configuration only once committed, so every voter reaches the same configuration. At `f = 0` the sole
/// voter self-elects and its append commits at once, so a change applies synchronously — the same code
/// path as a fleet, never a mode switch (R8). The voter set is the config group's own membership (fixed
/// here); it is distinct from the volume neighbourhood the admit/retire commands grow, and changes only
/// by joint-consensus membership change (owed).
pub struct ConfigGroup {
  raft: RaftNode,
  configuration: Configuration,
  applied: u64,
}

impl ConfigGroup {
  /// The solo configuration group — one voter, `owner`, `f = 0`, version zero (the laptop degenerate).
  /// The lone voter elects itself leader at once, so it may immediately propose configuration changes.
  pub fn solo(owner: HostId) -> ConfigGroup {
    let mut raft = RaftNode::new(owner, vec![owner]);
    let _ = raft.start_election();
    ConfigGroup {
      raft,
      configuration: Configuration::solo(owner),
      applied: 0,
    }
  }

  /// The current configuration — read per request; a request carrying an older version is refused with
  /// this one ([`Configuration::check_version`]).
  pub fn configuration(&self) -> &Configuration {
    &self.configuration
  }

  /// Whether this node is the configuration group's leader (only the leader may propose changes).
  pub fn is_leader(&self) -> bool {
    self.raft.is_leader()
  }

  /// The number of committed configuration-log entries applied so far (each a proposed change; a no-op
  /// change is not proposed, so the log's growth tracks real configuration changes — the near-zero
  /// commit rate the design makes a tripwire).
  pub fn log_len(&self) -> usize {
    self.raft.committed_entries().len()
  }

  /// Proposes a reconfiguration through the configuration log and applies whatever newly commits (§4.8;
  /// the Raft consensus). A no-op change (a member already present, or absent, in the neighbourhood) is
  /// not proposed. Returns whether the configuration changed. At `f = 0` the proposal commits at once, so
  /// the change applies before returning; a non-leader cannot propose and returns `false`.
  pub fn reconfigure(&mut self, change: Reconfiguration) -> bool {
    let (command, would_change) = match change {
      Reconfiguration::Admit(host) => (
        ConfigCommand::Admit(host),
        !self.configuration.neighbourhood.contains(&host),
      ),
      Reconfiguration::Retire(host) => (
        ConfigCommand::Retire(host),
        host != self.configuration.owner && self.configuration.neighbourhood.contains(&host),
      ),
    };
    if !would_change {
      return false;
    }
    self.propose(command)
  }

  /// Proposes `command` to the configuration log and applies whatever newly commits. Returns whether the
  /// configuration changed. A non-leader cannot append and returns `false`.
  fn propose(&mut self, command: ConfigCommand) -> bool {
    if !self.raft.append_command(command.encode()) {
      return false;
    }
    self.apply_committed()
  }

  /// Applies every committed but not-yet-applied log entry to the configuration, in commit order, so the
  /// configuration is the deterministic fold of the committed log. Returns whether anything changed.
  fn apply_committed(&mut self) -> bool {
    let mut changed = false;
    let committed = self.raft.committed_entries().to_vec();
    while let Some(entry) = committed.get(usize::try_from(self.applied).unwrap_or(usize::MAX)) {
      if let Some(command) = ConfigCommand::decode(&entry.command) {
        changed |= self.apply_change(command);
      }
      self.applied = self.applied.saturating_add(1);
    }
    changed
  }

  /// Applies one committed command to the configuration — the deterministic mutation every voter makes.
  /// Returns whether the configuration changed (bumping the version once when it does, so a request under
  /// the stale version is refused). A command that no longer applies to the current state (a takeover of
  /// a host a prior committed entry already replaced) is a safe no-op.
  fn apply_change(&mut self, command: ConfigCommand) -> bool {
    let changed = match command {
      ConfigCommand::Admit(host) => {
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
      ConfigCommand::Retire(host) => {
        if host == self.configuration.owner || !self.configuration.neighbourhood.contains(&host) {
          false
        } else {
          self.configuration.neighbourhood.retain(|h| *h != host);
          true
        }
      }
      ConfigCommand::TakeOver { dead, object } => self.apply_take_over(dead, object),
    };
    if changed {
      self.configuration.version = self.configuration.version.saturating_add(1);
    }
    changed
  }

  /// Applies a committed takeover: reassigns the volume to the rendezvous-first survivor, bumps the host
  /// epoch, and drops the dead owner. A no-op if `dead` is no longer the owner or no survivor remains
  /// (the caller validated both before proposing; this stays safe if the log order changed them).
  fn apply_take_over(&mut self, dead: HostId, object: u64) -> bool {
    if dead != self.configuration.owner {
      return false;
    }
    let survivors: Vec<HostId> = self
      .configuration
      .neighbourhood
      .iter()
      .copied()
      .filter(|host| *host != dead)
      .collect();
    let Some(successor) = rendezvous_first(&survivors, object) else {
      return false;
    };
    self.configuration.owner = successor;
    // Bump the authority so the dead owner's in-flight records are fenced (the next epoch, a monotonic
    // step like the version, never a tunable).
    self.configuration.host_epoch = HostEpoch(self.configuration.host_epoch.0.saturating_add(1));
    self.configuration.neighbourhood = survivors;
    true
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
    // Validate against the current configuration before proposing — a takeover of a non-owner or one
    // with no survivor is refused without a log entry.
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
    if rendezvous_first(&survivors, object).is_none() {
      return Err(TakeoverError::NoSurvivor);
    }
    // Propose the takeover through the configuration log; at f = 0 it commits and applies at once.
    self.propose(ConfigCommand::TakeOver { dead, object });
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

  /// The solo group self-elects, so it may propose; each real change is committed through the log (the
  /// committed log grows only for changes that alter the configuration, not for no-ops).
  #[test]
  fn changes_are_committed_through_the_log() {
    let mut group = ConfigGroup::solo(OWNER);
    assert!(
      group.is_leader(),
      "the lone voter self-elects and may propose"
    );
    assert_eq!(group.log_len(), 0, "nothing committed yet");

    assert!(group.reconfigure(Reconfiguration::Admit(A)));
    assert_eq!(group.log_len(), 1, "the admit committed one log entry");
    assert!(
      !group.reconfigure(Reconfiguration::Admit(A)),
      "a duplicate admit is a no-op"
    );
    assert_eq!(
      group.log_len(),
      1,
      "a no-op change is not proposed, so the log does not grow"
    );

    assert!(group.reconfigure(Reconfiguration::Retire(A)));
    assert_eq!(group.log_len(), 2, "the retire committed a second entry");
    assert_eq!(group.configuration().neighbourhood, vec![OWNER]);
  }

  /// Each config command round-trips through encode/decode, and a malformed entry decodes to `None`
  /// (applied as a safe no-op rather than panicking).
  #[test]
  fn config_command_round_trips() {
    let commands = [
      ConfigCommand::Admit(A),
      ConfigCommand::Retire(B),
      ConfigCommand::TakeOver {
        dead: OWNER,
        object: 0x1234,
      },
    ];
    for command in commands {
      assert_eq!(
        ConfigCommand::decode(&command.encode()),
        Some(command),
        "round-trip is identity"
      );
    }
    assert_eq!(
      ConfigCommand::decode(&[]),
      None,
      "empty bytes decode to nothing"
    );
    assert_eq!(
      ConfigCommand::decode(&[9, 9, 9]),
      None,
      "an unknown tag decodes to nothing"
    );
  }
}
