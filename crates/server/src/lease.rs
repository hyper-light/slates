//! The owner lease (§4.8 "Leases and reads"; AUD-08): the rule under which a node serves the **latest
//! state** of an object it owns — a read at the head, the head version, a status, a write — and refuses
//! it `LeaseUnconfirmed` while its authority is uncertain.
//!
//! **What it holds.** Epoch fencing alone does not authorize an owner-local read: the fence stops a stale
//! owner's *records* at the holders, not its answers to its own clients. An owner cut off from its
//! neighbourhood keeps its local live view; the surviving quorum takes its objects over and advances
//! them; the old owner's clients would read a past. The design's rule (§4.8): "only an owner with a
//! currently confirmed, conservatively bounded lease may serve the latest head locally. Expiry /
//! uncertainty stops those reads before a takeover can make them stale. Membership heartbeat arrival is
//! not a lease grant; a majority observation must belong to the relevant authority generation."
//!
//! **The lease.** An object's latest state is served while, within the last [`lease_bound_ns`] on this
//! host's clock, `f` of the object's **other candidate holders** (its copyset under the configuration it
//! was placed under) each **answered this node's direct probe**, the answer reporting this node **alive at
//! its current incarnation** and announcing the **same configuration version** this node has installed.
//! The owner is itself one of the `2f + 1` candidates, so with `f` others it holds `f + 1` — a quorum of
//! the copyset ([`OwnerLease::holds`]).
//!
//! **Why that is safe (the intersection).** A successor serves an object only after phase one over the
//! object's surviving candidates — `f + 1` promises out of the `2f` survivors. A holder answers a
//! promotion for a departed owner only once it has **not** answered that owner alive for the horizon
//! ([`AnswersGiven::promotion_open`]) — or once the owner has announced it saw the configuration that
//! retired it (then the owner refuses everything itself, and yields its objects on re-admission). Any
//! `f + 1` of the `2f` survivors intersect any `f` of them, so while the owner's lease holds, at least one
//! holder of every possible promotion quorum is still refusing the promotion: no successor adopts, so no
//! stale answer is possible. The owner's bound is the holder's less the clock-rate tolerance (twice
//! RFC 5905's 500 ppm: its own clock slow, the holder's fast), measured from the probe's **send** time —
//! before the holder formed its answer.
//!
//! **Pauses, expiry, takeover.** The lease is checked per request against the host's suspend-inclusive
//! monotonic clock (`slates_machine::clock`), never against a loop having run: a paused owner's lease
//! lapses by the clock *while* it is paused, and a shard other than the control shard reads the answers the
//! control shard fanned to it (`fleet::fan_configs_to_shards`) — absolute times, so a stale fan only
//! shortens the lease. An owner that learns of a newer configuration version than it has installed (from
//! any answer) is **superseded**: unconfirmed for every object until it installs that version; a retired
//! owner never installs it (it is no member), so it refuses until re-admitted, and on re-admission its
//! bumped host epoch reassigns its held objects' routing to their successors
//! (`FleetNode::install_configuration`).
//!
//! **Laptop.** `f = 0` needs no answers: the only candidate is the owner, nothing can take it over, and the
//! lease holds by the same arithmetic (R8: `needed = 0`, no branch). Immutable reads — a green's named
//! version, a pinned attachment's view — need no lease (§4.8: "Immutable complete snapshot reads need no
//! latest-head lease but still require read rights and verified content").
//!
//! Measured on a healthy loopback fleet (2026-09-19, this box): a confirmation arrives per peer per probe
//! period (100 ms at full health, 300 ms at the local-health cap) against a bound of 899.1 ms, and the
//! council's death-confirmation window (ten periods) exceeds the horizon, so the holder gate defers no
//! promotion of a genuinely dead owner; it binds when a holder's evidence of the owner is fresher than the
//! council's — a false retirement, or an owner that is alive but cut off from the council alone.

use std::collections::BTreeMap;

use slates_db::register::{HostId, Quorum};

use crate::daemon::HEARTBEAT_NS;
use crate::fleet::{LOCAL_HEALTH_CAP, SUSPICION_PERIODS};

/// Derived: RFC 5905 (NTPv4) §11.1 bounds a disciplined host clock's frequency error at 500 ppm (the
/// `PHI` maximum frequency tolerance); the owner's side of the bound is shortened by twice that — its own
/// clock slow, the holder's fast — so a lease measured on the owner's clock ends before the holder's gate
/// opens on the holder's.
const CLOCK_RATE_TOLERANCE_PPM: u64 = 500;

/// Format: parts per million.
const MILLION: u64 = 1_000_000;

/// Derived: the **membership horizon** — the longest a peer can go unanswered before this node's detector
/// declares it dead: the probe deadline plus the suspicion window ([`SUSPICION_PERIODS`] periods), each
/// period dilated to the Lifeguard local-health cap ([`LOCAL_HEALTH_CAP`] + 1). A holder that has not
/// answered an owner for this long is retiring it; an owner unanswered for this long is being retired. The
/// holder side of the lease ([`AnswersGiven::promotion_open`]).
pub fn horizon_ns() -> u64 {
  HEARTBEAT_NS
    .saturating_mul(u64::from(LOCAL_HEALTH_CAP.saturating_add(1)))
    .saturating_mul(u64::from(SUSPICION_PERIODS.saturating_add(1)))
}

/// Derived: the owner side of the lease — the horizon less twice the clock-rate tolerance
/// ([`CLOCK_RATE_TOLERANCE_PPM`]), so an owner whose clock runs slow against a holder whose clock runs
/// fast still stops serving before that holder answers a promotion.
pub fn lease_bound_ns() -> u64 {
  let horizon = horizon_ns();
  let tolerance = horizon
    .saturating_mul(CLOCK_RATE_TOLERANCE_PPM.saturating_mul(2))
    .saturating_div(MILLION);
  horizon.saturating_sub(tolerance)
}

/// One peer's latest answer that confirms this node: the direct acknowledgement of a probe this node
/// sent, reporting this node alive at its current incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Confirmation {
  /// When this node **sent** the probe the answer acknowledged (this host's monotonic clock).
  pub sent_ns: u64,
  /// The configuration version the answering peer announced with the answer.
  pub version: u64,
}

/// This node's lease evidence as an owner: written on the control shard by the probe tasks, fanned to
/// every shard each period, read per request by the verbs and the mount bridge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerLease {
  /// Each peer's latest confirming answer. Bounded by the members this node probes; pruned to the
  /// current members each period ([`OwnerLease::retain_members`]).
  pub confirmations: BTreeMap<HostId, Confirmation>,
  /// A configuration version newer than this node's installed one that a peer announced: this node's
  /// authority is uncertain (a retirement, a takeover, a change it has not applied) until it installs that
  /// version — cleared by [`OwnerLease::installed`].
  pub superseded: Option<u64>,
  /// When this node last installed a new configuration version (this host's monotonic clock; `None` before
  /// the first). For the membership horizon after it, the lease holds without `f` fresh confirmations — a
  /// **bounded startup allowance** ([`OwnerLease::holds`]), safe because a takeover of this node's objects
  /// cannot commit until the council has confirmed this node unreachable for its death-confirmation window,
  /// which exceeds the horizon: within a fresh configuration's first horizon no successor can yet exist, so
  /// this only spares a **reachable** owner a false refusal while its first acks under the new version
  /// accumulate. A node cut off long ago installed its configuration long ago and gets no allowance.
  pub configuration_installed_ns: Option<u64>,
}

impl OwnerLease {
  /// Records a confirming answer from `peer` to the probe sent at `sent_ns`, the peer announcing
  /// `version`. A newer answer replaces an older; an out-of-order older one is ignored.
  pub fn confirm(&mut self, peer: HostId, sent_ns: u64, version: u64) {
    let confirmation = self
      .confirmations
      .entry(peer)
      .or_insert(Confirmation { sent_ns, version });
    if sent_ns >= confirmation.sent_ns {
      *confirmation = Confirmation { sent_ns, version };
    }
  }

  /// A peer announced `version`, newer than this node's installed configuration.
  pub fn supersede(&mut self, version: u64) {
    self.superseded = Some(self.superseded.map_or(version, |known| known.max(version)));
  }

  /// This node installed configuration `version` at `now_ns`: a supersession at or below it is resolved,
  /// and the bounded startup allowance restarts from `now_ns` (see [`OwnerLease::configuration_installed_ns`]).
  pub fn installed(&mut self, version: u64, now_ns: u64) {
    if self.superseded.is_some_and(|newer| newer <= version) {
      self.superseded = None;
    }
    self.configuration_installed_ns = Some(now_ns);
  }

  /// The newest configuration version this node knows of — installed, or announced by a peer and not
  /// yet installed. Announced on this node's own probes, so a holder learns when a retired owner has seen
  /// its retirement.
  pub fn known_version(&self, installed: u64) -> u64 {
    self
      .superseded
      .map_or(installed, |newer| newer.max(installed))
  }

  /// Drops the answers of peers that are no longer members (a restarted peer's retired id), keeping the
  /// ledger bounded by the membership.
  pub fn retain_members(&mut self, members: &[HostId]) {
    self.confirmations.retain(|peer, _| members.contains(peer));
  }

  /// The decision: whether this node may serve the latest state of an object whose candidate holders
  /// are `candidates` (this node among them) at `now_ns` — not superseded, and either within the bounded
  /// startup allowance of the current configuration ([`OwnerLease::configuration_installed_ns`]) or with
  /// `f` of the other candidates having confirmed it within [`lease_bound_ns`] under the installed `version`.
  pub fn holds(
    &self,
    now_ns: u64,
    local: HostId,
    version: u64,
    quorum: Quorum,
    candidates: &[HostId],
  ) -> bool {
    if self.superseded.is_some() {
      return false;
    }
    let needed = usize::try_from(quorum.f).unwrap_or(usize::MAX);
    let bound = lease_bound_ns();
    let fresh = candidates
      .iter()
      .filter(|host| **host != local)
      .filter(|host| {
        self.confirmations.get(host).is_some_and(|confirmation| {
          confirmation.version == version && now_ns.saturating_sub(confirmation.sent_ns) <= bound
        })
      })
      .count();
    if fresh >= needed {
      return true;
    }
    // The bounded startup allowance: within the membership horizon of installing this configuration, no
    // successor can yet exist (the council's death-confirmation window exceeds the horizon), so a reachable
    // owner still gathering its first acks under the new version serves rather than false-refusing.
    self
      .configuration_installed_ns
      .is_some_and(|installed| now_ns.saturating_sub(installed) <= horizon_ns())
  }
}

/// The holder side of the lease: when this node last answered each peer's direct probe — a lease-confirming
/// acknowledgement that feeds that peer's lease over its own objects, since this node is one of its
/// candidate holders — and the newest configuration version each peer announced. The evidence that gates a
/// successor's promotion of a departed peer's objects at this holder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnswersGiven {
  /// When this node last answered `peer`'s direct probe (this host's monotonic clock): the acknowledgement
  /// confirms this node alive to `peer`, feeding `peer`'s lease, so until the horizon has passed since it,
  /// a lease `peer` built partly on this answer may still hold. Bounded by the members; pruned each period.
  pub alive_answers: BTreeMap<HostId, u64>,
  /// The newest configuration version each peer announced on its probes.
  pub announced_versions: BTreeMap<HostId, u64>,
}

impl AnswersGiven {
  /// This node answered `peer`'s direct probe at `now_ns` — a lease-confirming acknowledgement to `peer`.
  pub fn answered_alive(&mut self, peer: HostId, now_ns: u64) {
    let last = self.alive_answers.entry(peer).or_insert(now_ns);
    *last = (*last).max(now_ns);
  }

  /// `peer`'s probe announced `version` as the newest configuration it knows of.
  pub fn announced(&mut self, peer: HostId, version: u64) {
    let known = self.announced_versions.entry(peer).or_insert(version);
    *known = (*known).max(version);
  }

  /// Drops the entries of peers that are no longer members, keeping both maps bounded by the membership.
  pub fn retain_members(&mut self, members: &[HostId]) {
    self.alive_answers.retain(|peer, _| members.contains(peer));
    self
      .announced_versions
      .retain(|peer, _| members.contains(peer));
  }

  /// Whether this holder may answer a promotion of an object whose owner `departed` under configuration
  /// `since_version`: the departed owner has announced it knows that version or a newer one (it refuses
  /// its own clients now and yields its objects on re-admission), or this node has not reported it alive
  /// for the horizon — so its lease, if it had one from this node, has lapsed.
  pub fn promotion_open(&self, departed: HostId, since_version: u64, now_ns: u64) -> bool {
    let acknowledged = self
      .announced_versions
      .get(&departed)
      .is_some_and(|announced| *announced >= since_version);
    let silent = self
      .alive_answers
      .get(&departed)
      .is_none_or(|last| now_ns.saturating_sub(*last) > horizon_ns());
    acknowledged || silent
  }
}

/// An object whose owner a committed configuration retired, held here, awaiting the successor's
/// promotion — the departed owner and the configuration version that retired it, so the promotion gate
/// knows whom it answered and what the owner must have seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepartedOwner {
  /// The owner the configuration retired.
  pub owner: HostId,
  /// The configuration version that retired it.
  pub since_version: u64,
}

#[cfg(test)]
mod tests {
  use super::*;

  const LOCAL: HostId = HostId(1);
  const X: HostId = HostId(2);
  const Y: HostId = HostId(3);

  /// The owner's bound is strictly inside the holder's horizon (the clock-rate tolerance), and both are
  /// the detector's own horizon, not a number of their own.
  #[test]
  fn the_owner_bound_sits_inside_the_horizon() {
    assert!(lease_bound_ns() < horizon_ns());
    assert_eq!(
      horizon_ns(),
      HEARTBEAT_NS * u64::from(LOCAL_HEALTH_CAP + 1) * u64::from(SUSPICION_PERIODS + 1)
    );
    assert!(horizon_ns() - lease_bound_ns() <= horizon_ns() / 500);
  }

  /// At `f = 1` one fresh confirmation from another candidate holds the lease; none does not; a
  /// confirmation older than the bound, or under another configuration version, does not count; `f = 2`
  /// needs two fresh; `f = 0` holds with nothing (the laptop). The configuration was installed in the
  /// distant past, so the startup allowance never applies here.
  #[test]
  fn f_other_fresh_same_version_confirmations_hold_the_lease() {
    let candidates = [LOCAL, X, Y];
    let one = Quorum { f: 1 };
    let mut lease = OwnerLease::default();
    let now = 10 * lease_bound_ns();
    lease.configuration_installed_ns = Some(0); // installed long ago: no startup allowance
    assert!(!lease.holds(now, LOCAL, 4, one, &candidates));
    assert!(lease.holds(now, LOCAL, 4, Quorum { f: 0 }, &[LOCAL]));

    lease.confirm(X, now - lease_bound_ns(), 4);
    let at_bound = lease.holds(now, LOCAL, 4, one, &candidates);
    let past_bound = lease.holds(now + 1, LOCAL, 4, one, &candidates);
    let wrong_version = lease.holds(now, LOCAL, 5, one, &candidates);
    assert!(at_bound, "exactly at the bound still holds");
    assert!(!past_bound, "one past it lapses");
    assert!(!wrong_version, "wrong version does not count");

    lease.confirm(Y, now, 4);
    let two = Quorum { f: 2 };
    let five = [LOCAL, X, Y, HostId(4), HostId(5)];
    assert!(
      lease.holds(now + 1, LOCAL, 4, one, &candidates),
      "the fresher Y confirms"
    );
    assert!(
      !lease.holds(now + 1, LOCAL, 4, two, &five),
      "f = 2 needs two fresh: X has lapsed"
    );
  }

  /// A supersession voids every object's lease until the newer version is installed; a fresh confirmation
  /// under the wrong version does not count, and one under the installed version restores the lease.
  #[test]
  fn a_supersession_voids_the_lease_until_the_newer_version_is_installed() {
    let candidates = [LOCAL, X, Y];
    let one = Quorum { f: 1 };
    let mut lease = OwnerLease::default();
    let now = 10 * lease_bound_ns();
    lease.configuration_installed_ns = Some(0); // installed long ago: no startup allowance
    lease.confirm(Y, now, 4);
    assert!(
      lease.holds(now, LOCAL, 4, one, &candidates),
      "a fresh Y confirms version 4"
    );

    lease.supersede(5);
    assert!(
      !lease.holds(now, LOCAL, 4, one, &candidates),
      "superseded overrides the fresh Y"
    );
    lease.installed(4, 0);
    assert!(
      lease.superseded.is_some(),
      "installing an older version resolves nothing"
    );
    lease.installed(5, 0);
    assert!(lease.superseded.is_none());
    assert!(
      !lease.holds(now, LOCAL, 5, one, &candidates),
      "the answers were at version 4"
    );
    lease.confirm(Y, now, 5);
    assert!(lease.holds(now, LOCAL, 5, one, &candidates));
  }

  /// The bounded startup allowance holds the lease for the horizon after a configuration install, with no
  /// confirmations, then lapses; a supersession voids it even inside the allowance.
  #[test]
  fn the_startup_allowance_holds_briefly_then_the_strict_rule_applies() {
    let candidates = [LOCAL, X, Y];
    let one = Quorum { f: 1 };
    let mut lease = OwnerLease::default();
    let installed = 10 * horizon_ns();
    lease.installed(3, installed);
    assert!(
      lease.holds(installed, LOCAL, 3, one, &candidates),
      "just installed: allowed"
    );
    assert!(
      lease.holds(installed + horizon_ns(), LOCAL, 3, one, &candidates),
      "at the horizon: allowed"
    );
    assert!(
      !lease.holds(installed + horizon_ns() + 1, LOCAL, 3, one, &candidates),
      "past the horizon with no confirmation: unconfirmed"
    );
    lease.supersede(4);
    assert!(
      !lease.holds(installed, LOCAL, 3, one, &candidates),
      "superseded even inside the allowance"
    );
  }

  /// An older answer never overwrites a newer one; pruning keeps only members.
  #[test]
  fn confirmations_keep_the_newest_and_prune_to_members() {
    let mut lease = OwnerLease::default();
    lease.confirm(X, 50, 1);
    lease.confirm(X, 40, 2);
    assert_eq!(
      lease.confirmations.get(&X),
      Some(&Confirmation {
        sent_ns: 50,
        version: 1
      })
    );
    lease.confirm(Y, 60, 1);
    lease.retain_members(&[LOCAL, Y]);
    assert_eq!(lease.confirmations.len(), 1);
    assert!(lease.confirmations.contains_key(&Y));
    assert_eq!(lease.known_version(3), 3);
    lease.supersede(7);
    assert_eq!(lease.known_version(3), 7);
  }

  /// A holder answers a promotion of a departed owner's object only once the owner is silent for the
  /// horizon or has announced the retiring version; a fresh alive answer under an older announced version
  /// keeps the gate closed.
  #[test]
  fn a_promotion_waits_for_the_departed_owners_silence_or_acknowledgement() {
    let mut given = AnswersGiven::default();
    let now = 10 * horizon_ns();
    assert!(given.promotion_open(X, 3, now), "never answered: open");
    given.answered_alive(X, now);
    given.announced(X, 2);
    assert!(
      !given.promotion_open(X, 3, now),
      "answered alive just now, at an older version"
    );
    assert!(
      !given.promotion_open(X, 3, now + horizon_ns()),
      "at the horizon: still closed"
    );
    assert!(
      given.promotion_open(X, 3, now + horizon_ns() + 1),
      "past it: open"
    );
    given.announced(X, 3);
    assert!(
      given.promotion_open(X, 3, now),
      "the owner saw its retirement: open at once"
    );
    given.announced(X, 1);
    assert_eq!(
      given.announced_versions.get(&X),
      Some(&3),
      "announcements only advance"
    );
    given.retain_members(&[LOCAL]);
    assert!(given.alive_answers.is_empty() && given.announced_versions.is_empty());
  }
}
