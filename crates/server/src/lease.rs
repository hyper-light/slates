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
//! host's clock, enough of the object's **other candidate holders** each **answered this node's direct
//! probe**, the answer reporting this node **alive at its current incarnation** and naming this node under the
//! **authority standing** it holds ([`Standing`]: the versions its settled and its current neighbourhoods were
//! fixed at). Enough is `f` at the candidate floor: the owner is itself one of the `2f + 1` candidates, so with
//! `f` others it holds `f + 1` — a quorum of the copyset ([`OwnerLease::verdict`], [`confirmations_needed`]).
//! While this owner's neighbourhood change is in flight its records are written jointly, and its lease is too:
//! enough confirmations in **each** cohort ([`slates_db::register::Configuration::lease_cohorts`]).
//!
//! **Why that is safe (the intersection).** A successor serves an object only after phase one over the
//! object's recovery cohort — at the floor, `f + 1` promises out of the `2f` survivors. A holder answers a
//! promotion for a departed owner only once it has **not** answered that owner alive for the horizon
//! ([`AnswersGiven::promotion_open`]) — or once the owner has announced it saw the configuration that
//! retired it (then the owner refuses everything itself, and yields its objects on re-admission). Any
//! `f + 1` of the `2f` survivors intersect any `f` of them, so while the owner's lease holds, at least one
//! holder of every possible promotion quorum is still refusing the promotion: no successor adopts, so no
//! stale answer is possible. Below the floor — a cohort a forming region fixed with fewer hosts — a promotion
//! needs fewer promises (`cohort − f`, at least one: [`Quorum::recovery`]), so the same intersection needs
//! every other candidate up to `f`, and a lone owner, with no other candidate, needs none. A candidate the
//! owner's configuration has retired can promise nothing, so it is not asked for a confirmation either.
//! Demanding `f` of a lone owner refused it its own objects for good once its startup allowance ran out
//! (`docs/bugs/2026-09-29-a-lone-owner-refused-its-own-objects.md`); counting `f + 1` promises in a two-host
//! cohort lost records its survivor held, and the lease's bound moved with the corrected promotion
//! (`docs/bugs/2026-09-29-a-formation-cohort-lost-a-record-its-survivor-held.md`). The joint lease closes a
//! gap the settled cohort alone left: a takeover recovers through the owner's settled neighbourhood as the
//! council holds it, which the owner's report may have moved to its current one before the owner installed
//! that; an owner cut off together with holders that had not learned it either kept its lease on their answers
//! while a successor recovered through the new cohort, whose members it never asked. The owner's bound is the
//! holder's less the clock-rate tolerance (twice RFC 5905's 500 ppm: its own clock slow, the holder's fast),
//! measured from the probe's **send** time — before the holder formed its answer.
//!
//! **Pauses, expiry, takeover.** The lease is checked per request against the host's suspend-inclusive
//! monotonic clock (`slates_machine::clock`), never against a loop having run: a paused owner's lease
//! lapses by the clock *while* it is paused, and a shard other than the control shard reads the answers the
//! control shard fanned to it (`fleet::fan_configs_to_shards`) — absolute times, so a stale fan only
//! shortens the lease. An owner is **superseded** — unconfirmed for every object until it installs a newer
//! configuration — only by an answer its standing cannot account for ([`OwnerLease::answered`]): a holder
//! whose newer configuration no longer holds it as a member (its retirement), or fixes its settled
//! neighbourhood later than its current one (its id retired and admitted again). A retired owner never
//! installs a configuration holding it (it is no member), so it refuses until re-admitted, and on
//! re-admission its bumped host epoch reassigns its held objects' routing to their successors
//! (`FleetNode::install_configuration`). Any other configuration change — another host's admission,
//! settlement, retirement or confirmed takeover — changes nothing here. Keyed to the regional version instead,
//! every such change voided every owner's lease until it installed the change: a takeover's successor refused
//! its own adopted volume `NFS3ERR_JUKEBOX` in 5 of 17 Linux io_uring runs, each counted
//! `lease.refused.superseded`
//! (`docs/bugs/2026-09-29-the-owner-lease-was-voided-by-other-hosts-configuration-changes.md`).
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

use slates_db::register::{HostId, Quorum, Standing};

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
  /// The version the answering peer's configuration fixed this node's settled neighbourhood at: the
  /// confirmation counts while this node's standing recognizes it ([`Standing::recognizes`]).
  pub standing: u64,
}

/// This node's lease evidence as an owner: written on the control shard by the probe tasks, fanned to
/// every shard each period, read per request by the verbs and the mount bridge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerLease {
  /// Each peer's latest confirming answer. Bounded by the members this node probes; pruned to the
  /// current members each period ([`OwnerLease::retain_members`]).
  pub confirmations: BTreeMap<HostId, Confirmation>,
  /// The version of a configuration a holder answered under that this node's standing cannot account for —
  /// one no longer holding it as a member (its retirement), or fixing its settled neighbourhood later than its
  /// current one (its id retired and admitted again): its authority is uncertain until it installs that
  /// version — cleared by [`OwnerLease::installed`] ([`OwnerLease::answered`]).
  pub superseded: Option<u64>,
  /// When this node last installed a new configuration version (this host's monotonic clock; `None` before
  /// the first). For the membership horizon after it, the lease holds without fresh confirmations — a
  /// **bounded startup allowance** ([`OwnerLease::verdict`]), safe because a takeover of this node's objects
  /// cannot commit until the council has confirmed this node unreachable for its death-confirmation window,
  /// which exceeds the horizon: within a fresh configuration's first horizon no successor can yet exist, so
  /// this only spares a **reachable** owner a false refusal while its first acks under the new version
  /// accumulate. A node cut off long ago installed its configuration long ago and gets no allowance.
  pub configuration_installed_ns: Option<u64>,
}

impl OwnerLease {
  /// Records a confirming answer from `peer` to the probe sent at `sent_ns`, the peer's configuration fixing
  /// this node's settled neighbourhood at `standing`. A newer answer replaces an older; an out-of-order older
  /// one is ignored.
  pub fn confirm(&mut self, peer: HostId, sent_ns: u64, standing: u64) {
    let confirmation = self
      .confirmations
      .entry(peer)
      .or_insert(Confirmation { sent_ns, standing });
    if sent_ns >= confirmation.sent_ns {
      *confirmation = Confirmation { sent_ns, standing };
    }
  }

  /// Folds `peer`'s answer to the probe this node sent at `sent_ns` (§4.8 "Leases and reads"): the answering
  /// configuration's `version` and its view of this node's standing (`standing`, the version it fixed this
  /// node's settled neighbourhood at, `None` when this node is no member of it), against this node's own
  /// `own` standing under its `installed` configuration.
  /// - A standing this node recognizes — its settled neighbourhood, or its current one the council has settled
  ///   since — confirms it ([`OwnerLease::confirm`]).
  /// - A standing newer than its current neighbourhood, or no membership under a configuration newer than the
  ///   one it installed, means its authority changed where it has not looked: superseded until it installs
  ///   that version ([`OwnerLease::supersede`]).
  /// - Anything older — a holder that has not installed this node's admission or its settlement yet — is no
  ///   evidence either way, and changes nothing.
  pub fn answered(
    &mut self,
    peer: HostId,
    sent_ns: u64,
    version: u64,
    standing: Option<u64>,
    own: Standing,
    installed: u64,
  ) {
    match standing {
      Some(generation) if own.recognizes(generation) => self.confirm(peer, sent_ns, generation),
      Some(generation) if own.superseded_by(generation) => self.supersede(version),
      None if version > installed => self.supersede(version),
      Some(_) | None => {}
    }
  }

  /// An answer under configuration `version` showed this node's authority changed there.
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

  /// The newest configuration version this node knows of — installed, or one an answer showed its authority
  /// changed in and not yet installed. Announced on this node's own probes, so a holder learns when a retired
  /// owner has seen its retirement.
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

  /// The decision: whether this node, standing as `own`, may serve the latest state of an object whose lease
  /// `cohorts` are given ([`slates_db::register::Configuration::lease_cohorts`]: one, or the settled and the
  /// current one while a change is in flight) at `now_ns`. It holds when not superseded and, in **every**
  /// cohort, enough of the other candidates that are still `members` confirmed this node within
  /// [`lease_bound_ns`] under a standing it recognizes ([`confirmations_needed`]) — or within the bounded
  /// startup allowance of the current configuration ([`OwnerLease::configuration_installed_ns`]). A refusal
  /// says which of the two it is, so a refused read can be told apart in the status counts.
  pub fn verdict(
    &self,
    now_ns: u64,
    local: HostId,
    own: Standing,
    quorum: Quorum,
    cohorts: &[Vec<HostId>],
    members: &[HostId],
  ) -> LeaseVerdict {
    if let Some(newer) = self.superseded {
      return LeaseVerdict::Superseded { newer };
    }
    // The bounded startup allowance: within the membership horizon of installing this configuration, no
    // successor can yet exist (the council's death-confirmation window exceeds the horizon), so a reachable
    // owner still gathering its first acks under the new version serves rather than false-refusing.
    let allowed = self
      .configuration_installed_ns
      .is_some_and(|installed| now_ns.saturating_sub(installed) <= horizon_ns());
    if allowed {
      return LeaseVerdict::Holds;
    }
    let bound = lease_bound_ns();
    for cohort in cohorts {
      let live_others: Vec<&HostId> = cohort
        .iter()
        .filter(|host| **host != local && members.contains(host))
        .collect();
      let needed = confirmations_needed(live_others.len(), cohort.len(), quorum);
      let fresh = live_others
        .iter()
        .filter(|host| {
          self.confirmations.get(host).is_some_and(|confirmation| {
            own.recognizes(confirmation.standing)
              && now_ns.saturating_sub(confirmation.sent_ns) <= bound
          })
        })
        .count();
      if fresh < needed {
        return LeaseVerdict::Unconfirmed { fresh, needed };
      }
    }
    LeaseVerdict::Holds
  }
}

/// Whether an owner's lease holds on an object, and if not, why ([`OwnerLease::verdict`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseVerdict {
  /// The owner may serve the object's latest state.
  Holds,
  /// A holder answered under configuration `newer`, which this node has not installed, in a way its standing
  /// cannot account for (its retirement, or its id admitted again): its authority is uncertain until it does.
  Superseded {
    /// The newest such version.
    newer: u64,
  },
  /// Past the startup allowance, in some lease cohort fewer of the object's other live candidates confirmed
  /// this node within the bound, under a standing it recognizes, than the intersection needs.
  Unconfirmed {
    /// The fresh confirmations counted in that cohort.
    fresh: usize,
    /// The confirmations it needs ([`confirmations_needed`]).
    needed: usize,
  },
}

impl LeaseVerdict {
  /// Whether the lease holds.
  pub fn holds(self) -> bool {
    self == LeaseVerdict::Holds
  }

  /// The status count a refusal on this verdict is recorded under, `None` when the lease holds.
  pub fn refusal_count_name(self) -> Option<&'static str> {
    match self {
      LeaseVerdict::Holds => None,
      LeaseVerdict::Superseded { .. } => Some(LEASE_SUPERSEDED),
      LeaseVerdict::Unconfirmed { .. } => Some(LEASE_UNCONFIRMED),
    }
  }
}

/// The status count under which an owner records a latest-state request it refused while superseded
/// ([`LeaseVerdict::Superseded`]).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub const LEASE_SUPERSEDED: &str = "lease.refused.superseded";

/// The status count under which an owner records a latest-state request it refused past its startup allowance
/// because too few of the object's other candidates confirmed it ([`LeaseVerdict::Unconfirmed`]).
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub const LEASE_UNCONFIRMED: &str = "lease.refused.unconfirmed";

/// Derived: how many of the `live_others` in a lease cohort of `cohort` hosts — its candidate holders other
/// than the owner that are still members — must have freshly confirmed the owner for its lease to hold there:
/// as many as leave no promotion quorum without one. A successor adopts only on [`Quorum::recovery`] promises
/// from the cohort — `cohort − f`, at least one, counted over the whole cohort because a retired candidate may
/// have acknowledged a commit, but drawn from the live others, since neither the retired owner nor a retired
/// candidate promises (§4.8 "Promotion"; `crate::takeover`) — and a holder that confirmed the owner refuses to
/// promise until that lease can have lapsed ([`AnswersGiven::promotion_open`]). The confirmations meet every
/// such quorum once more than `live_others − promises` of them are fresh: `f` at the candidate floor with every
/// candidate live (`2f` others, as before), every other candidate in a smaller cohort, where a single promise
/// already promotes, and none once the live others are too few to promise at all — a lone owner, or a cohort
/// whose other hosts have all retired — since no successor can then adopt through it. A lone owner therefore
/// keeps serving its own objects past its startup allowance
/// (`docs/bugs/2026-09-29-a-lone-owner-refused-its-own-objects.md`), and `f = 0` (the laptop) needs none by the
/// same arithmetic (R8). The earlier `others − f` matched a promotion rule of `f + 1` promises, which declared
/// a record lost while its formation cohort's survivor held it; with the recovery quorum it would have let an
/// owner settled beside one host serve on no confirmation while that host alone promoted a successor
/// (`docs/bugs/2026-09-29-a-formation-cohort-lost-a-record-its-survivor-held.md`).
pub fn confirmations_needed(live_others: usize, cohort: usize, quorum: Quorum) -> usize {
  live_others
    .saturating_add(1)
    .saturating_sub(quorum.recovery(cohort))
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

  /// A settled owner's standing: both neighbourhoods fixed at `generation`.
  fn settled_at(generation: u64) -> Standing {
    Standing {
      settled: generation,
      current: generation,
    }
  }

  /// An owner lease whose configuration was installed long ago, so the startup allowance never applies.
  fn past_the_allowance() -> OwnerLease {
    OwnerLease {
      configuration_installed_ns: Some(0),
      ..OwnerLease::default()
    }
  }

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
  /// confirmation older than the bound, or naming a standing this owner does not hold, does not count; `f = 2`
  /// needs two fresh; `f = 0` holds with nothing (the laptop).
  #[test]
  fn f_other_fresh_recognized_confirmations_hold_the_lease() {
    let cohort = [vec![LOCAL, X, Y]];
    let members = [LOCAL, X, Y, HostId(4), HostId(5)];
    let one = Quorum { f: 1 };
    let own = settled_at(4);
    let mut lease = past_the_allowance();
    let now = 10 * lease_bound_ns();
    assert!(
      !lease
        .verdict(now, LOCAL, own, one, &cohort, &members)
        .holds()
    );
    assert!(
      lease
        .verdict(now, LOCAL, own, Quorum { f: 0 }, &[vec![LOCAL]], &members)
        .holds()
    );

    lease.confirm(X, now - lease_bound_ns(), 4);
    let at_bound = lease.verdict(now, LOCAL, own, one, &cohort, &members);
    let past_bound = lease.verdict(now + 1, LOCAL, own, one, &cohort, &members);
    let other_standing = lease.verdict(now, LOCAL, settled_at(5), one, &cohort, &members);
    assert!(at_bound.holds(), "exactly at the bound still holds");
    assert!(!past_bound.holds(), "one past it lapses");
    assert_eq!(
      other_standing,
      LeaseVerdict::Unconfirmed {
        fresh: 0,
        needed: 1
      },
      "a confirmation naming another standing does not count"
    );

    lease.confirm(Y, now, 4);
    let five = [vec![LOCAL, X, Y, HostId(4), HostId(5)]];
    assert!(
      lease
        .verdict(now + 1, LOCAL, own, one, &cohort, &members)
        .holds(),
      "the fresher Y confirms"
    );
    assert!(
      !lease
        .verdict(now + 1, LOCAL, own, Quorum { f: 2 }, &five, &members)
        .holds(),
      "f = 2 needs two fresh: X has lapsed"
    );
  }

  /// Below the candidate floor the intersection asks for every other candidate up to `f`: a successor needs
  /// [`Quorum::recovery`] promises of the object's cohort, which a cohort of `f` or fewer others meets with a single
  /// promise (every commit there is held by all of them), so the owner needs each of them fresh; a lone owner has
  /// none to ask and needs none. At the floor it is `f`, as before.
  #[test]
  fn below_the_candidate_floor_the_owner_needs_every_other_candidate_up_to_f() {
    let one = Quorum { f: 1 };
    let two = Quorum { f: 2 };
    let own = settled_at(4);
    let members = [LOCAL, X, Y, HostId(4), HostId(5)];
    let mut lease = past_the_allowance();
    let now = 10 * lease_bound_ns();
    let holds = |lease: &OwnerLease, quorum, cohort: &[HostId]| {
      lease
        .verdict(now, LOCAL, own, quorum, &[cohort.to_vec()], &members)
        .holds()
    };
    assert!(
      holds(&lease, one, &[LOCAL]),
      "alone at f = 1: no successor can be promoted"
    );
    assert!(
      !holds(&lease, one, &[LOCAL, X]),
      "beside one host at f = 1: that host's promise alone promotes, so it must confirm"
    );
    lease.confirm(X, now, 4);
    assert!(
      holds(&lease, one, &[LOCAL, X]),
      "its fresh confirmation holds the lease"
    );
    assert!(
      holds(&lease, one, &[LOCAL, X, Y]),
      "at the floor f = 1 needs one"
    );
    let four = [LOCAL, X, Y, HostId(4)];
    assert!(
      !holds(&lease, two, &four),
      "three others at f = 2: two promises promote, so one confirmation leaves two unconfirmed"
    );
    lease.confirm(Y, now, 4);
    assert!(
      holds(&lease, two, &four),
      "two fresh of the three others meet every pair of promises"
    );
    assert!(
      holds(&lease, two, &[LOCAL, X, Y, HostId(4), HostId(5)]),
      "at the floor f = 2 needs two"
    );
  }

  /// A candidate the owner's configuration has retired promises nothing, so it is not waited for. An owner
  /// settled beside one host that has since retired needs no confirmation there, and neither does a floor
  /// cohort with one of its two other candidates retired: recovery through it needs `3 − 1 = 2` promises and
  /// one live candidate is left, so no successor can adopt through it. What the survivor can promote through is
  /// the owner's current cohort, which the change moved to it and the joint lease asks: there it must confirm.
  #[test]
  fn a_retired_candidate_is_not_waited_for() {
    let one = Quorum { f: 1 };
    let own = Standing {
      settled: 4,
      current: 6,
    };
    let mut lease = past_the_allowance();
    let now = 10 * lease_bound_ns();
    let members = [LOCAL, Y];
    assert!(
      lease
        .verdict(now, LOCAL, own, one, &[vec![LOCAL, X]], &members)
        .holds(),
      "the retired X can promise nothing: no promotion through this cohort exists"
    );
    assert!(
      lease
        .verdict(now, LOCAL, own, one, &[vec![LOCAL, X, Y]], &members)
        .holds(),
      "one live candidate cannot give the two promises this cohort's recovery needs"
    );
    let joint = [vec![LOCAL, X, Y], vec![LOCAL, Y]];
    assert_eq!(
      lease.verdict(now, LOCAL, own, one, &joint, &members),
      LeaseVerdict::Unconfirmed {
        fresh: 0,
        needed: 1
      },
      "through the current cohort Y alone promotes, so it must confirm"
    );
    lease.confirm(Y, now, 6);
    assert!(
      lease
        .verdict(now, LOCAL, own, one, &joint, &members)
        .holds()
    );
  }

  /// §4.8 "Leases and reads" with joint writes: while an owner's change is in flight, a takeover may recover
  /// through its settled cohort or — once the council takes its report — through its current one, so the lease
  /// needs the intersection in each. Do: confirm only the settled cohort's other candidate, then only the
  /// current cohort's. Expect: neither alone holds the lease, both together do. Before the joint lease an
  /// owner cut off together with the settled cohort's holders kept its lease on their answers while a
  /// successor recovered through the current cohort, whose members it never asked.
  #[test]
  fn a_change_in_flight_needs_confirmations_in_both_cohorts() {
    let one = Quorum { f: 1 };
    let (old, new) = (HostId(2), HostId(3));
    let own = Standing {
      settled: 4,
      current: 6,
    };
    let cohorts = [vec![LOCAL, old], vec![LOCAL, new]];
    let members = [LOCAL, old, new];
    let now = 10 * lease_bound_ns();
    let mut settled_only = past_the_allowance();
    settled_only.confirm(old, now, 4);
    assert_eq!(
      settled_only.verdict(now, LOCAL, own, one, &cohorts, &members),
      LeaseVerdict::Unconfirmed {
        fresh: 0,
        needed: 1
      },
      "the settled cohort's confirmation alone does not hold the lease"
    );
    let mut current_only = past_the_allowance();
    current_only.confirm(new, now, 6);
    assert!(
      !current_only
        .verdict(now, LOCAL, own, one, &cohorts, &members)
        .holds(),
      "the current cohort's confirmation alone does not hold it either"
    );
    settled_only.confirm(new, now, 6);
    assert!(
      settled_only
        .verdict(now, LOCAL, own, one, &cohorts, &members)
        .holds(),
      "one in each holds it, the second naming the settlement the council took"
    );
  }

  /// §4.8 "Leases and reads" ("a majority observation must belong to the relevant authority generation"): an
  /// answer supersedes the owner only when its standing cannot account for it. Do: fold answers under newer
  /// configurations that recognize the owner, one that fixes a newer settled neighbourhood for it, one that no
  /// longer holds it as a member, and older ones. Expect: another host's change supersedes nothing and confirms;
  /// the owner's retirement and its id's re-admission supersede it until installed; a holder that has not
  /// admitted it yet changes nothing. Keyed to the regional version, a successor refused its adopted volume in 5
  /// of 17 Linux io_uring runs, every refusal `lease.refused.superseded`.
  #[test]
  fn only_a_change_to_the_owners_own_authority_supersedes_it() {
    let one = Quorum { f: 1 };
    let own = settled_at(4);
    let cohort = [vec![LOCAL, X, Y]];
    let members = [LOCAL, X, Y];
    let now = 10 * lease_bound_ns();
    let installed = 9;
    let mut lease = past_the_allowance();
    lease.answered(X, now, 12, Some(4), own, installed);
    assert_eq!(
      lease.superseded, None,
      "a newer configuration that holds the owner as it is"
    );
    assert!(
      lease
        .verdict(now, LOCAL, own, one, &cohort, &members)
        .holds(),
      "and its answer confirms"
    );
    lease.answered(Y, now, 7, None, own, installed);
    assert_eq!(
      lease.superseded, None,
      "a holder that has not admitted the owner yet"
    );
    lease.answered(Y, now, 13, None, own, installed);
    assert_eq!(
      lease.superseded,
      Some(13),
      "a newer configuration without it: retired"
    );
    assert_eq!(
      lease.verdict(now, LOCAL, own, one, &cohort, &members),
      LeaseVerdict::Superseded { newer: 13 }
    );
    lease.installed(13, now);
    assert_eq!(
      lease.superseded, None,
      "installing it resolves the supersession"
    );
    let mut readmitted = past_the_allowance();
    readmitted.answered(X, now, 20, Some(18), own, installed);
    assert_eq!(
      readmitted.superseded,
      Some(20),
      "a settled neighbourhood newer than its current one: its id admitted again"
    );
    let mut lagging = past_the_allowance();
    lagging.answered(X, now, 3, Some(2), own, installed);
    assert_eq!(lagging.superseded, None, "an older standing is no evidence");
    assert!(
      !lagging
        .verdict(now, LOCAL, own, one, &cohort, &members)
        .holds(),
      "and confirms nothing"
    );
  }

  /// The bounded startup allowance holds the lease for the horizon after a configuration install, with no
  /// confirmations, then lapses; a supersession voids it even inside the allowance.
  #[test]
  fn the_startup_allowance_holds_briefly_then_the_strict_rule_applies() {
    let cohort = [vec![LOCAL, X, Y]];
    let members = [LOCAL, X, Y];
    let one = Quorum { f: 1 };
    let own = settled_at(3);
    let mut lease = OwnerLease::default();
    let installed = 10 * horizon_ns();
    lease.installed(3, installed);
    let holds = |lease: &OwnerLease, now| {
      lease
        .verdict(now, LOCAL, own, one, &cohort, &members)
        .holds()
    };
    assert!(holds(&lease, installed), "just installed: allowed");
    assert!(
      holds(&lease, installed + horizon_ns()),
      "at the horizon: allowed"
    );
    assert!(
      !holds(&lease, installed + horizon_ns() + 1),
      "past the horizon with no confirmation: unconfirmed"
    );
    lease.supersede(4);
    assert!(
      !holds(&lease, installed),
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
        standing: 1
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

  /// Shape: the largest `f` the exhaustive intersection oracle enumerates — cohorts up to `2f + 1 = 7` hosts,
  /// so every subset of an owner's other candidates is a 6-bit mask.
  const ORACLE_MAX_F: u32 = 3;

  /// The subsets of `hosts` hosts (as bit masks) with exactly `size` members.
  fn subsets_of_size(hosts: usize, size: usize) -> impl Iterator<Item = u32> {
    (0..1u32 << hosts).filter(move |mask| mask.count_ones() as usize == size)
  }

  /// §4.8 "Leases and reads" (the intersection the lease rests on): for every `f` up to [`ORACLE_MAX_F`], every
  /// cohort a region can fix and every set of its other candidates still live, any [`confirmations_needed`] of
  /// the live others meet every promotion quorum a successor can gather among them ([`Quorum::recovery`] of the
  /// whole cohort) — so while the lease holds, some holder of every such quorum is still refusing — and one
  /// confirmation fewer leaves some quorum wholly unconfirmed. A promotion quorum and the lease are both drawn
  /// from the live others: neither the retired owner nor a retired candidate promises.
  #[test]
  fn every_promotion_quorum_meets_the_lease_confirmations_and_no_fewer_suffice() {
    for f in 0..=ORACLE_MAX_F {
      let quorum = Quorum { f };
      for cohort in 1..=quorum.candidates() {
        let others = cohort - 1;
        let promises = quorum.recovery(cohort);
        for live in 0..=others {
          let needed = confirmations_needed(live, cohort, quorum);
          let quorums: Vec<u32> = subsets_of_size(live, promises).collect();
          for confirmed in subsets_of_size(live, needed) {
            for promised in &quorums {
              assert_ne!(
                confirmed & promised,
                0,
                "f = {f}, cohort {cohort}, {live} live others: {needed} confirmations meet every promotion of \
                 {promises}"
              );
            }
          }
          if needed > 0 {
            let unconfirmed = subsets_of_size(live, needed - 1)
              .any(|confirmed| quorums.iter().any(|promised| confirmed & promised == 0));
            assert!(
              unconfirmed,
              "f = {f}, cohort {cohort}, {live} live others: {} confirmations leave a promotion unmet",
              needed - 1
            );
          }
        }
      }
    }
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
