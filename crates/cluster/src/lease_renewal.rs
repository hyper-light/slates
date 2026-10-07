//! The owner lease's renewal (§4.8 "Leases and reads"; AUD-08): the probes an owner sends its neighbourhood so its
//! lease is confirmed at the lease's own cadence, not at the failure detector's.
//!
//! **What it fixes.** An owner serves its objects' latest state only while enough of each object's other candidate
//! holders answered one of its probes sent within the lease bound (`slates-server` `lease.rs`). Those answers came
//! only from the detector's probes, and the detector probes one member a period, in rotation over every member it
//! knows (SWIM §3.1), each period as long as its probe needs. Two Docker networks joined by a router shaping 100 ms
//! ± 40 ms one way (2026-10-07, `docs/wip/bench/multiregion/run.sh`) put three far members in every round of a
//! six-member fleet: the owner's two near holders last answered 966 ms and 1,362 ms before a read, against a
//! 900 ms bound, so the owner refused its own objects (`LeaseUnconfirmed`) while every member was alive, and every
//! read forwarded to it from the other region was refused. The simulated plane reproduces it: a near holder went
//! 9,886 ms unanswered among four far members (`tests/member_plane.rs`).
//!
//! **The rule.** A lease is renewed by its holder at a cadence set by the lease, never by an unrelated schedule (Gray
//! and Cheriton, "Leases", SOSP 1989; Chubby's KeepAlive, Burrows, OSDI 2006; a Raft leader's lease rides its
//! per-period heartbeats, Ongaro 2014 §6.4). So each step the plane probes every holder it has not probed within one
//! renewal interval — the detector's own probe of a holder counts — and an answer feeds the lease exactly as the
//! detector's acknowledgement does, timed from its own send. The holders are the owner's neighbourhood, settled and
//! current, which bounds the work by the scatter width.
//!
//! **Why it is as safe as before.** A renewal is the same probe on the same plane: the holder answers it through the
//! fleet, which records when it answered (the holder side's promotion gate), and the owner counts it from its send,
//! before the holder formed its answer. Nothing about the bound changes; only how often it is renewed.
//!
//! **Answers matched to their own send.** An answer is credited with the send time of the renewal it echoes, never a
//! later one, or a lease would outlive its evidence. So renewal nonces come from their own range, disjoint from the
//! detector's ([`RENEWAL_NONCE_BASE`]), and each holder keeps the renewals that can still confirm: those sent within
//! one bound ([`LeaseRenewal::outstanding`]). The disjointness is checked, not assumed: a detector nonce reaching the
//! range stops renewals for good, counted ([`Renewals::exhausted`]).

use std::collections::{BTreeMap, VecDeque};

use slates_db::register::HostId;

/// Format: the first renewal nonce. The detector numbers its probes and relays from zero, one per probe, so its
/// nonces stay below this for 2⁶³ probes (at one a microsecond, about 292,000 years); a renewal's are at or above it,
/// and a detector nonce that ever reaches it stops renewals ([`Renewals::detector_nonce`]).
pub const RENEWAL_NONCE_BASE: u64 = 1 << 63;

/// How the owner renews its lease: how often, and for how long an answer can still confirm it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeaseRenewal {
  /// How often each holder is probed at least, nanoseconds.
  pub interval_ns: u64,
  /// The lease bound, nanoseconds: an answer to a probe sent longer ago than this confirms nothing.
  pub bound_ns: u64,
}

impl LeaseRenewal {
  /// The renewals one holder keeps awaiting an answer: every one sent within one bound, since an older one's answer
  /// confirms nothing. At least one.
  pub fn outstanding(&self) -> usize {
    let per_bound = self.bound_ns.div_ceil(self.interval_ns.max(1));
    usize::try_from(per_bound).unwrap_or(usize::MAX).max(1)
  }
}

/// One holder's renewal state: when this node last probed it (either way), and its renewals awaiting an answer.
#[derive(Debug, Default)]
struct Holder {
  last_probed_ns: Option<u64>,
  outstanding: VecDeque<(u64, u64)>,
}

/// A renewal to send: its target and nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Renewal {
  /// The holder to probe.
  pub to: HostId,
  /// The probe's nonce, which its answer echoes.
  pub nonce: u64,
}

/// The owner's renewal state over its holders. Bounded by the holders the fleet names each step.
#[derive(Debug)]
pub struct Renewals {
  policy: LeaseRenewal,
  holders: BTreeMap<HostId, Holder>,
  next_nonce: u64,
  exhausted: bool,
}

impl Renewals {
  /// No holders yet.
  pub fn new(policy: LeaseRenewal) -> Renewals {
    Renewals {
      policy,
      holders: BTreeMap::new(),
      next_nonce: RENEWAL_NONCE_BASE,
      exhausted: false,
    }
  }

  /// Whether renewals have stopped: a detector nonce reached the renewal range, or the range ran out.
  pub fn exhausted(&self) -> bool {
    self.exhausted
  }

  /// The detector sent probe `nonce` to `to` at `now_ns`: it renews `to` as well as any renewal would, and its nonce
  /// must stay below the renewal range.
  pub fn detector_probed(&mut self, to: HostId, nonce: u64, now_ns: u64) {
    if nonce >= RENEWAL_NONCE_BASE {
      self.exhausted = true;
    }
    if let Some(holder) = self.holders.get_mut(&to) {
      holder.last_probed_ns = Some(now_ns);
    }
  }

  /// The renewals due at `now_ns` to `holders` (this node's neighbourhood, itself excluded by the caller), each
  /// recorded as sent then; holders no longer named are forgotten.
  pub fn due(&mut self, now_ns: u64, holders: &[HostId], out: &mut Vec<Renewal>) {
    self.holders.retain(|host, _| holders.contains(host));
    if self.exhausted {
      return;
    }
    let outstanding = self.policy.outstanding();
    for host in holders {
      let holder = self.holders.entry(*host).or_default();
      let due = holder
        .last_probed_ns
        .is_none_or(|last| now_ns.saturating_sub(last) >= self.policy.interval_ns);
      if !due {
        continue;
      }
      let nonce = self.next_nonce;
      let Some(next) = nonce.checked_add(1) else {
        self.exhausted = true;
        return;
      };
      self.next_nonce = next;
      if holder.outstanding.len() >= outstanding {
        holder.outstanding.pop_front();
      }
      holder.outstanding.push_back((nonce, now_ns));
      holder.last_probed_ns = Some(now_ns);
      out.push(Renewal { to: *host, nonce });
    }
  }

  /// The send time of this node's renewal `nonce` to `from`, taken out: `None` for a nonce that is no renewal of
  /// `from`'s still awaited.
  pub fn answered(&mut self, from: HostId, nonce: u64) -> Option<u64> {
    if nonce < RENEWAL_NONCE_BASE {
      return None;
    }
    let holder = self.holders.get_mut(&from)?;
    let at = holder
      .outstanding
      .iter()
      .position(|(held, _)| *held == nonce)?;
    holder.outstanding.remove(at).map(|(_, sent_ns)| sent_ns)
  }

  /// When the next renewal falls due, on the owner's clock (`None`: no holder, or renewals stopped).
  pub fn next_due(&self) -> Option<u64> {
    if self.exhausted {
      return None;
    }
    self
      .holders
      .values()
      .map(|holder| {
        holder
          .last_probed_ns
          .map_or(0, |last| last.saturating_add(self.policy.interval_ns))
      })
      .min()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a renewal every 100 ms within a 900 ms bound, the daemon's coordinator period and lease bound.
  const POLICY: LeaseRenewal = LeaseRenewal {
    interval_ns: 100,
    bound_ns: 900,
  };

  /// §4.8 (AUD-08). Do: ask for renewals to two holders over time, the detector probing one of them in between.
  /// Expect: each holder is renewed once an interval has passed since it was last probed either way, the detector's
  /// probe postpones that holder's renewal, and the next due time is the earliest holder's.
  #[test]
  fn each_holder_is_renewed_an_interval_after_it_was_last_probed() {
    let (a, b) = (HostId(2), HostId(3));
    let mut renewals = Renewals::new(POLICY);
    let mut out = Vec::new();
    renewals.due(0, &[a, b], &mut out);
    assert_eq!(out.iter().map(|r| r.to).collect::<Vec<_>>(), vec![a, b]);
    out.clear();
    renewals.detector_probed(a, 5, 60);
    renewals.due(100, &[a, b], &mut out);
    assert_eq!(out.iter().map(|r| r.to).collect::<Vec<_>>(), vec![b]);
    assert_eq!(renewals.next_due(), Some(160));
  }

  /// §4.8 (AUD-08): an answer is credited with its own renewal's send. Do: renew one holder three times, then answer
  /// the second renewal, a detector nonce, the same nonce again, and a renewal of another holder. Expect: only the
  /// second renewal's answer is credited, with its own send time, and once.
  #[test]
  fn an_answer_is_credited_with_the_send_of_the_renewal_it_echoes_once() {
    let (a, b) = (HostId(2), HostId(3));
    let mut renewals = Renewals::new(POLICY);
    let mut out = Vec::new();
    for now in [0, 100, 200] {
      renewals.due(now, &[a, b], &mut out);
    }
    let second_of_a = out.iter().filter(|r| r.to == a).nth(1).copied().unwrap();
    let first_of_b = out.iter().find(|r| r.to == b).copied().unwrap();
    assert_eq!(renewals.answered(a, second_of_a.nonce), Some(100));
    assert_eq!(
      renewals.answered(a, second_of_a.nonce),
      None,
      "credited once"
    );
    assert_eq!(
      renewals.answered(a, 7),
      None,
      "a detector nonce is not a renewal"
    );
    assert_eq!(
      renewals.answered(a, first_of_b.nonce),
      None,
      "another holder's renewal"
    );
  }

  /// §4.8 (AUD-08): only renewals that can still confirm are kept. Do: renew one holder for longer than a bound
  /// without answers. Expect: it keeps one bound's worth, the oldest dropped first.
  #[test]
  fn a_holder_keeps_only_the_renewals_sent_within_one_bound() {
    let a = HostId(2);
    let mut renewals = Renewals::new(POLICY);
    let mut out = Vec::new();
    for step in 0..20 {
      renewals.due(step * 100, &[a], &mut out);
    }
    let first = out.first().copied().unwrap();
    let last = out.last().copied().unwrap();
    assert_eq!(
      renewals.answered(a, first.nonce),
      None,
      "dropped past the bound"
    );
    assert_eq!(renewals.answered(a, last.nonce), Some(1_900));
    assert_eq!(POLICY.outstanding(), 9);
  }

  /// The disjointness of the nonce ranges is checked. Do: report a detector probe whose nonce reached the renewal
  /// range. Expect: no renewal is due any more, and none is next.
  #[test]
  fn a_detector_nonce_in_the_renewal_range_stops_renewals() {
    let a = HostId(2);
    let mut renewals = Renewals::new(POLICY);
    let mut out = Vec::new();
    renewals.detector_probed(a, RENEWAL_NONCE_BASE, 0);
    renewals.due(1_000, &[a], &mut out);
    assert!(out.is_empty());
    assert!(renewals.exhausted());
    assert_eq!(renewals.next_due(), None);
  }

  /// A holder the fleet stops naming is forgotten. Do: renew two holders, then name one. Expect: the other's
  /// renewal is no longer answerable.
  #[test]
  fn a_holder_no_longer_named_is_forgotten() {
    let (a, b) = (HostId(2), HostId(3));
    let mut renewals = Renewals::new(POLICY);
    let mut out = Vec::new();
    renewals.due(0, &[a, b], &mut out);
    let of_b = out.iter().find(|r| r.to == b).copied().unwrap();
    renewals.due(10, &[a], &mut out);
    assert_eq!(renewals.answered(b, of_b.nonce), None);
  }
}
