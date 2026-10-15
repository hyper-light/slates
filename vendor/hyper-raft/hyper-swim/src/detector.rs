//! The SWIM failure detector, its timing measured (`docs/timing.md` §2.7): the protocol-period
//! machine that probes members and drives the [`Membership`] view from alive to suspect to dead.
//! Sans-io: it is fed `now`, acknowledgements and pings, and returns the pings and ping-requests to
//! send and the time to be polled again ([`wake`](Detector::wake)).
//!
//! Evidence: SWIM (Das, Gupta and Motivala, DSN 2002) for the probe, the indirect probe, the
//! suspicion and infection-style dissemination; Lifeguard (Dadgar, Phillips and Currey, DSN 2018)
//! for the buddy system, the relay's nack and local health; Chen, Toueg and Aguilera (2002) for the
//! detector each probe stream is; `docs/research/timing.md` holds what each establishes.
//!
//! **Each pair is an NFD-E detector.** A member's probes to one peer and that peer's
//! acknowledgements are a heartbeat stream on the member's own clock: probe `k` sent at `s_k`,
//! answered at `A_k`, so NFD-E's delay `A_k − σ_k` is the probe's round trip, with no second clock
//! in it. A [`LinkEstimator`] per peer holds its mean, variance, loss, correlation and window, and
//! [`arrival_detector_at`] chooses the margin `α` that minimizes unavailability at the pair's probe
//! interval, any margin: a pair's probes never overlap, so each is judged alone, and a false
//! condemnation is priced at what the owner states it costs
//! ([`Detector::set_condemnation_cost`]). A probe's acknowledgement is due at `s + μ + α`; if none
//! came, the indirect probe asks relays, each of which judges its own leg to the peer by its own
//! deadline and nacks when that passes unanswered, and a probe answered by neither suspects the
//! peer. The period is what its probe needs: the direct deadline, and the relay stage when the
//! direct passed unanswered, until every relay asked has nacked or the slowest has its response due
//! by the bound it stated (SWIM §3.1: the protocol's properties hold for the average period).
//!
//! **Before a pair's own estimator configures.** A pair's estimator refuses until it has its
//! evidence ([`Refusal`]). Its probes are then judged by the member's pooled estimator, every round
//! trip the member measured on the paths the pool fits (the hosts' stalls, which the traces found
//! dominate, are in it), where the pool's span covers the pair; and otherwise by the pair's own
//! retransmission timer, RFC 6298's over its round trips: the initial second before any, then
//! `SRTT + max(G, 4·RTTVAR)`, at least RFC 6298's second until the pool has measured the host's
//! stalls and floored by the pool's margin after, doubled at each expiry. Every probe is judged: a
//! timer-judged probe's period ends at its answer, or at its deadline unanswered, and then it
//! suspects as any judged probe does. The handshake that keyed a pair, where its owner gives one
//! ([`Detector::join_measured`]), is its timer's first round trip.
//!
//! **Dead.** A suspected peer is told by the member's next probe to it (Lifeguard's buddy system);
//! it is condemned when that probe also goes unanswered, and only once the member has since had an
//! answer from someone else, so a member whose own network has failed condemns nobody (Lifeguard's
//! local health, decided by evidence). So a member of a two-member view never condemns: the one
//! other member is the suspect, and a lone survivor cannot tell its peer's death from its own
//! network's failure, which is the case Lifeguard's local health exists for; each side of a
//! partition of two would otherwise condemn the other. An owner that must act on a death in a pair
//! takes its evidence from outside the detector: a quorum, or its supervisor's word that the
//! process ended. A gossiped suspicion is a hint: only a member's own probes condemn. A member with nobody alive or suspected left probes the members it holds dead and
//! tells each so; a live one refutes in its answer.
//!
//! **What it found, and why.** A probe states its deadline as it is sent ([`Ping::due_ns`]), and
//! each poll states what the member's own probes found ([`Detector::findings`]): a suspicion, a
//! condemnation made pending, a condemnation, each with its evidence: the probe, the deadline
//! stated when it was sent, when its period ended with no answer delivered, and for a
//! condemnation the answer from another member that proved this member's network. An owner can
//! say why a member was suspected or condemned, and a test can trace every one to this rule.
//!
//! **The member's own lateness** is measured, not multiplied: every wake the member is late for is
//! folded into its granularity `G` ([`Wakes`]), which floors the margins, and is never below the
//! resolution of the clock its owner reads, so a member whose wakes all read exactly on time still
//! configures ([`Detector::new`]); the member's own delay in reading acknowledgements is in the
//! round trips it measures; and a probe is resolved when the member wakes, with every
//! acknowledgement delivered by then, so a late member does not blame its peers for its own
//! lateness. A round trip measured before the member has a wake and a period measured is taken by
//! no estimator, and counted ([`Detector::unmeasured`]).
//!
//! **The view is bounded and forgets the dead.** It holds at most the members the owner's placement
//! says this node can know ([`Detector::new`]'s `members`), and refuses an update about one more,
//! typed ([`Full`], counted by [`Detector::refused`]); every map keyed by a member holds only members
//! the view holds. A dead member's record is kept while gossip of it from before its death can still
//! arrive: SWIM's dissemination budget `T = λ ln n` periods past this member's adoption of the death,
//! each period as long as the longest this member runs ([`Detector::detection_bound`]'s period).
//! Then it is forgotten, with everything held of the member; a record past its window also makes
//! room for a newcomer at once. A member with nobody alive or suspected left keeps its dead: they are
//! the members it probes. `docs/timing.md` §2.7 and `docs/research/swim.md` give the derivation and
//! what is left of the risk.
//!
//! Coordinate-aware indirect probing: the detector carries a Vivaldi coordinate engine
//! ([`crate::coordinates`]) fed by the round trips it measures and the coordinates it learns for
//! peers ([`learn_coordinate`](Detector::learn_coordinate)). Indirect-probe relays are chosen
//! nearest the target in coordinate space, with a deterministic id order where coordinates are
//! unknown.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::time::Duration;

use hyper_timing::{
    Arrivals, Costs, Exposure, LinkBehaviour, LinkEstimator, Refusal, Schedule, Wakes,
    arrival_detector_at, lateness_bound,
};

use crate::HostId;

use crate::codec::Coordinate;
use crate::coordinates::{CoordinateEngine, NetworkCoordinate};
use crate::extension::{ExtensionDecision, ExtensionDenial, ExtensionTracker};
use crate::gossip::Gossip;
use crate::membership::{Change, Full, Liveness, MemberState, Membership};

/// A ping to send to `to` — the period's probe of it, or a probe relayed for another member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ping {
    /// The member to probe.
    pub to: HostId,
    /// The nonce the acknowledgement echoes.
    pub nonce: u64,
    /// When its answer is due, on the caller's clock, as stated when it is sent: for the period's
    /// probe, its time plus the `μ + α` of the verdict judging it; for a probe relayed for another
    /// member, this member's own deadline for the target ([`Detector::on_ping_req`]), at which the
    /// owner nacks if no answer has come.
    pub due_ns: u64,
}

/// A judged probe that went unanswered, as its period ended: the evidence for what it found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unanswered {
    /// The member probed.
    pub target: HostId,
    /// The probe's nonce, which an answer echoes.
    pub nonce: u64,
    /// When it was sent, on the caller's clock.
    pub sent_ns: u64,
    /// When its answer was due, as stated when it was sent ([`Ping::due_ns`]).
    pub due_ns: u64,
    /// When the relays' responses were due, where the direct deadline passed and relays were asked
    /// ([`PingReq`]): the slowest relay's, by this member's span for it and the bound it stated.
    pub relays_due_ns: Option<u64>,
    /// When its period ended with no answer delivered, direct or relayed: at the relays' deadline,
    /// or before it once every relay asked had nacked.
    pub ended_ns: u64,
}

/// What a [`poll`](Detector::poll) found of a peer by this member's own probes, with its
/// evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finding {
    /// The probe went unanswered and its target, alive in this member's view, is suspected:
    /// counted in [`PeerReport::suspicions`].
    Suspected(Unanswered),
    /// The probe carried this member's suspicion of its target, telling it so, and went
    /// unanswered: the target's condemnation is pending, from the probe's end, on an answer from
    /// another member ([`PeerReport::pending_since_ns`]).
    Pending(Unanswered),
    /// `target`, its condemnation pending since `pending_since_ns`, is condemned at `at_ns`:
    /// `answered` answered this member's probe `nonce`, so this member's own network works.
    /// Counted in [`PeerReport::condemnations`].
    Condemned {
        /// The member condemned.
        target: HostId,
        /// When its told probe's period ended unanswered ([`Finding::Pending`]).
        pending_since_ns: u64,
        /// The member whose answer proved this member's network.
        answered: HostId,
        /// The probe it answered.
        nonce: u64,
        /// When the answered probe's period ended, condemning.
        at_ns: u64,
    },
}

/// An acknowledgement to send to `to` — the reply to a received [`Ping`] — and what it states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    /// The member that pinged us.
    pub to: HostId,
    /// How long after a ping-request from `to` reaches this member it responds, with the target's
    /// answer or a nack, nanoseconds: the longest deadline it holds for a probe of its own of any
    /// member but `to`, plus `G`, how late its wakes come on average, since it nacks at a wake. The
    /// acknowledgement carries it
    /// ([`SwimMessage::Ack`](crate::codec::SwimMessage::Ack)), so a prober that asks this member to
    /// relay knows when the response is due ([`Detector::on_ack`]).
    pub relay_within_ns: u64,
}

/// A ping-request: ask `relay` to ping `target` on our behalf and relay the acknowledgement back,
/// echoing `nonce`. SWIM sends these when a direct ping goes unanswered, so a lost packet on the
/// direct path is not taken for a failed member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PingReq {
    /// The peer asked to probe on our behalf.
    pub relay: HostId,
    /// The member to probe indirectly.
    pub target: HostId,
    /// The nonce of the probe the request is for.
    pub nonce: u64,
}

/// The detector configured for a probe: NFD-E's margin at the pair's interval, and what it
/// promises (`docs/timing.md` §2.7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    /// The mean round trip `μ`, the expected arrival's offset from the probe.
    pub round_trip: Duration,
    /// The margin `α` past it: the acknowledgement is due at `s + μ + α`.
    pub margin: Duration,
    /// The pair's probe interval `η` it was configured at: the round, `m` periods.
    pub interval: Duration,
    /// The loss the configurator was fed: lost, or later than anything the history has seen.
    pub loss: f64,
    /// Theorem 7's bound on the probability that a probe of a live peer goes unanswered by its
    /// deadline, `(V + p·α²)/(V + α²)`: the margin holds one probe.
    pub mistake: f64,
}

impl Verdict {
    /// `μ + α` in nanoseconds: from the probe to its deadline.
    fn span_ns(&self) -> u64 {
        nanos(self.round_trip.saturating_add(self.margin))
    }
}

/// RFC 6298 §2.2's timeout for a path with one measured round trip `R`, in round trips:
/// `SRTT = R`, `RTTVAR = R/2`, `RTO = SRTT + 4·RTTVAR = 3R`. A round trip past it is one the RFC's
/// own timer would have declared lost.
const RTO_ROUND_TRIPS: u64 = 3;

/// Whether the pool's verdict, whose span is `pooled_span`, covers `peer`: the pair's latest round
/// trip (its latest answer's, however late; a reused record's bound; before any answer, the
/// handshake's) is within the span. A pair with no round trip measured is not covered: nothing
/// shows the pool's paths are its path, and judged by the pool a peer joined by gossip before it was
/// spoken to was suspected, alive, before its first answer could return from 200 ms away. The
/// latest evidence decides, both ways: a pair one stall made late is the pool's to judge again from
/// its next answer in time, where a mark set once kept it out for good (slates, 2026-10-10), and a
/// pair whose path lengthens leaves the pool's judgment at its first late answer.
fn covered(peer: &Peer, pooled_span: u64) -> bool {
    peer.path_rtt_ns.is_some_and(|rtt| rtt <= pooled_span)
}

/// Whether `peer`'s path is another than the pool's: its latest round trip is past RFC 6298's
/// timeout ([`RTO_ROUND_TRIPS`]) of the pool's round trip, `pooled_rtt` ([`Detector::pool_round_trip`]).
/// Such a pair's round trips do not feed the pool. The mean, not the span: a margin grows with the
/// MTBF until a span covers a path a hundred times the pool's (§2.7), and the far-link runs showed
/// what a far path's samples do to the pool.
fn far(peer: &Peer, pooled_rtt: Option<u64>) -> bool {
    peer.path_rtt_ns
        .zip(pooled_rtt)
        .is_some_and(|(rtt, pooled)| rtt > pooled.saturating_mul(RTO_ROUND_TRIPS))
}

/// The verdict that judges a probe of `peer`: the pair's own once its estimator configures; before
/// that, the pool's where the pool has one and its span covers the pair ([`covered`]); otherwise the
/// pair's own retransmission timer's ([`timer_verdict`]), with `loss` the member's probe loss and
/// `granularity_ns` its `G`. With it, whether the verdict is the timer's, a provisional one. Every
/// probe is judged: the first form held probes as measurement only until an estimator configured,
/// so a peer silent before one did was judged by nothing (slates, 2026-10-10: a pair's own estimator
/// refused at 141 samples, its pool at 5, and the peer was judged by nothing for 4,000 periods). The
/// one rule [`Detector::verdict`] reports and a probe is judged by.
fn judging(
    peer: &Peer,
    pooled: Option<Verdict>,
    loss: f64,
    granularity_ns: u64,
) -> (Verdict, bool) {
    if let Some(own) = peer.stream.verdict {
        return (own, false);
    }
    if let Some(pooled) = pooled
        && covered(peer, pooled.span_ns())
    {
        return (pooled, false);
    }
    (timer_verdict(peer, pooled, loss, granularity_ns), true)
}

/// RFC 6298 (2.1)'s timeout before any round trip of the exchange is measured, nanoseconds:
/// "Until a round-trip time (RTT) measurement has been made ... the sender SHOULD set RTO <- 1
/// second"; RFC 8961 (1): "In the absence of any knowledge about the latency of a path, the initial
/// RTO MUST be conservatively set to no less than 1 second".
const INITIAL_RTO_NS: u64 = 1_000_000_000;

/// RFC 6298 (2.4)'s least timeout, nanoseconds: "Whenever RTO is computed, if it is less than 1
/// second, then the RTO SHOULD be rounded up to 1 second", because "a large minimum RTO is needed to
/// keep TCP conservative and avoid spurious retransmissions" (Allman and Paxson 1999). RFC 8961 lifts
/// it from a timer whose observations cover its path's behaviour (§5: "there is no minimum RTO
/// specified"). A pair's timer floors its timeout at it until the member's pool has measured the
/// host's stalls, which a timer over a few of the pair's round trips has not seen (§2.6, item 3: a
/// young history underestimates the variance by an order of magnitude until it has seen a stall);
/// from then the pool's margin, that measurement, floors its variance term instead
/// ([`timer_verdict`]).
const MINIMUM_RTO_NS: u64 = 1_000_000_000;

/// RFC 6298 (2.5)'s cap on a backed-off timeout, nanoseconds: "A maximum value MAY be placed on RTO
/// provided it is at least 60 seconds."
const MAXIMUM_RTO_NS: u64 = 60_000_000_000;

/// RFC 6298 (2.3)'s gain on the smoothed round trip, `α = 1/8`, as a right shift.
const SMOOTHED_GAIN_SHIFT: u32 = 3;

/// RFC 6298 (2.3)'s gain on the round trip's variation, `β = 1/4`, as a right shift.
const VARIATION_GAIN_SHIFT: u32 = 2;

/// RFC 6298 (2.2)'s `K`: the timeout is the smoothed round trip plus `K` variations.
const VARIATION_MULTIPLE: u64 = 4;

/// RFC 6298's retransmission timer over a pair's round trips, its probes and their answers
/// ([`Peer::rto`]). A round trip's nonce names the probe it answers, so a late answer is no
/// ambiguous sample and is taken (Karn's rule, RFC 6298 §3; RFC 8961 (2)(d)).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rto {
    /// `SRTT`, once a round trip was taken.
    smoothed_ns: Option<u64>,
    /// `RTTVAR`.
    variation_ns: u64,
    /// The timer's expiries since the latest round trip taken: each doubles the timeout (5.5), and a
    /// round trip taken recomputes it from the estimates (the RFC's §5, after (5.7)).
    backoff: u32,
    /// Round trips taken.
    samples: u64,
    /// Of them, answers that came after their probe's period had ended.
    late: u64,
    /// The longest taken.
    longest_ns: u64,
}

impl Rto {
    /// Takes a round trip (RFC 6298 (2.2) for the first, (2.3) after), `late` when its probe's
    /// period had already ended.
    fn take(&mut self, rtt_ns: u64, late: bool) {
        match self.smoothed_ns {
            None => {
                self.smoothed_ns = Some(rtt_ns);
                self.variation_ns = rtt_ns >> 1;
            }
            Some(smoothed) => {
                let error = smoothed.abs_diff(rtt_ns);
                // A value less a right shift of itself stays at or above zero, and each sum is at
                // most the larger of the two values it weighs: neither saturates.
                self.variation_ns = self
                    .variation_ns
                    .saturating_sub(self.variation_ns >> VARIATION_GAIN_SHIFT)
                    .saturating_add(error >> VARIATION_GAIN_SHIFT);
                self.smoothed_ns = Some(
                    smoothed
                        .saturating_sub(smoothed >> SMOOTHED_GAIN_SHIFT)
                        .saturating_add(rtt_ns >> SMOOTHED_GAIN_SHIFT),
                );
            }
        }
        self.backoff = 0;
        self.samples = self.samples.saturating_add(1);
        if late {
            self.late = self.late.saturating_add(1);
        }
        self.longest_ns = self.longest_ns.max(rtt_ns);
    }

    /// The timer expired with no answer: the next timeout doubles (5.5).
    fn expired(&mut self) {
        self.backoff = self.backoff.saturating_add(1);
    }

    /// The timeout: `SRTT + max(floor, K·RTTVAR)` (2.2, the floor RFC 6298's `G`), at least
    /// `minimum_ns` (2.4); before any round trip, the initial second (2.1); doubled at each
    /// expiry since the latest round trip (5.5), a doubling at most to the 60 s cap (2.5).
    fn timeout_ns(&self, floor_ns: u64, minimum_ns: u64) -> u64 {
        let computed = self.smoothed_ns.map_or(INITIAL_RTO_NS, |smoothed| {
            smoothed.saturating_add(
                self.variation_ns
                    .saturating_mul(VARIATION_MULTIPLE)
                    .max(floor_ns),
            )
        });
        let mut timeout = computed.max(minimum_ns);
        let cap = MAXIMUM_RTO_NS.max(timeout);
        for _ in 0..self.backoff {
            if timeout >= cap {
                break;
            }
            timeout = timeout.saturating_mul(2).min(cap);
        }
        timeout
    }

    fn report(&self, floor_ns: u64, minimum_ns: u64) -> TimerReport {
        TimerReport {
            timeout: Duration::from_nanos(self.timeout_ns(floor_ns, minimum_ns)),
            samples: self.samples,
            late: self.late,
            longest: Duration::from_nanos(self.longest_ns),
        }
    }
}

/// The verdict of a pair no estimator judges: its retransmission timer's (RFC 6298, [`Rto`]), the
/// expected arrival its smoothed round trip and the margin the rest of its timeout. The timer's
/// variance term is floored by the member's granularity `G`, as RFC 6298 floors it by its clock's,
/// and where the pool has measured the host's stalls, by the pool's margin, since the stalls are in
/// every pair's round trip (§2.6); until the pool has, the timeout is floored by RFC 6298 (2.4)'s
/// second ([`MINIMUM_RTO_NS`]). Before any round trip of the pair, its timeout is the initial second
/// (2.1) and all of it is margin. It promises no bound (`mistake` 1): Theorem 7's needs the pair's
/// own variance, which a timer over a few round trips has not measured; and no interval configures
/// it. `loss` sizes the relays an unanswered probe asks.
fn timer_verdict(peer: &Peer, pooled: Option<Verdict>, loss: f64, granularity_ns: u64) -> Verdict {
    let (floor, minimum) = timer_floors(pooled, granularity_ns);
    let timeout = peer.rto.timeout_ns(floor, minimum);
    let round_trip = peer
        .rto
        .smoothed_ns
        .map_or(0, |smoothed| smoothed.min(timeout));
    Verdict {
        round_trip: Duration::from_nanos(round_trip),
        margin: Duration::from_nanos(timeout.saturating_sub(round_trip)),
        interval: Duration::ZERO,
        loss,
        mistake: 1.0,
    }
}

/// A pair's retransmission timer's floors: under its variance term the member's granularity and,
/// where the pool has measured the host's stalls, the pool's margin; under its timeout RFC 6298
/// (2.4)'s second until the pool has measured them ([`MINIMUM_RTO_NS`]).
fn timer_floors(pooled: Option<Verdict>, granularity_ns: u64) -> (u64, u64) {
    match pooled {
        Some(pooled) => (granularity_ns.max(nanos(pooled.margin)), 0),
        None => (granularity_ns, MINIMUM_RTO_NS),
    }
}

/// What an answer changed of how its pair is judged ([`Detector::on_ack`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answered {
    /// Its own verdict, or the pool's or its timer's judgement: the pair is judged again.
    Judge,
    /// Only its stated relay bound.
    Restated,
    /// Nothing.
    Same,
}

impl Answered {
    fn of(judge: bool, restated: bool) -> Self {
        match (judge, restated) {
            (true, _) => Self::Judge,
            (false, true) => Self::Restated,
            (false, false) => Self::Same,
        }
    }
}

/// What one peer adds to its member's extremes ([`Extremes`]), as its pair was last judged
/// ([`Detector::respan`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Contribution {
    /// The span `μ + α` of the verdict judging the pair, where the member probes the peer.
    span_ns: Option<u64>,
    /// When a relay request to the peer has its response due, where it may be asked to relay
    /// ([`relay_due_ns`]).
    relay_due_ns: Option<u64>,
    /// Where the pair's timer judges it: the most `G` can be without changing its span
    /// ([`timer_g_limit`]).
    timer_g_ns: Option<u64>,
    /// Whether the pool's verdict can change how the pair is judged: it has no verdict of its own.
    follows_pool: bool,
}

/// A member's extremes over its peers' contributions ([`Contribution`]), nanoseconds: the two longest
/// spans of the verdicts judging the pairs it probes, and whose they are, which give its longest
/// deadline for a probe of its own with any one pair left out ([`Extremes::without`]); the latest any
/// relay it could ask has its response due; and the pool's verdict, by its epoch, and `G` its
/// contributions were judged at. Kept as each contribution changes ([`Extremes::change`]), and taken again from every
/// contribution only after one that held an extreme shrank ([`Detector::rescan`]), so a period's work
/// does not grow with the peers it holds. The first form judged every peer at each period's start,
/// and at 256 members a period took eight times main's (`docs/benchmarks.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extremes {
    first: u64,
    of: Option<HostId>,
    second: u64,
    second_of: Option<HostId>,
    relay: u64,
    relay_of: Option<HostId>,
    /// At least the second-latest relay due: the slowest relay's due falls to it without the
    /// extremes being taken again, as it falls at each restated bound.
    relay_second: u64,
    /// The least `G` above which some timer's span changes; `u64::MAX` while no timer judges.
    g_limit: u64,
    g_ns: u64,
    pool_epoch: u64,
    /// Whether a contribution that held an extreme has shrunk since they were taken: they are taken
    /// again before they are read.
    stale: bool,
}

impl Default for Extremes {
    fn default() -> Self {
        Self {
            first: 0,
            of: None,
            second: 0,
            second_of: None,
            relay: 0,
            relay_of: None,
            relay_second: 0,
            g_limit: u64::MAX,
            g_ns: 0,
            pool_epoch: 0,
            stale: false,
        }
    }
}

impl Extremes {
    /// Takes `member`'s contribution changing from `old` to `new`: what it raises is kept at once,
    /// and an extreme it held and shrinks from leaves the extremes stale.
    fn change(&mut self, member: HostId, old: Contribution, new: Contribution) {
        if let Some(limit) = new.timer_g_ns {
            self.g_limit = self.g_limit.min(limit);
        }
        self.stale |= self.shrinks(member, old, new);
        if self.stale {
            return;
        }
        if let Some(span) = new.span_ns {
            self.raise_span(member, span);
        }
        if let Some(due) = new.relay_due_ns {
            self.raise_relay(member, due);
        }
    }

    /// Whether `member`'s change from `old` to `new` takes an extreme it held below what the rest are
    /// known to reach: a span of the two longest that shrinks, or the slowest relay's due falling
    /// past the second's bound.
    fn shrinks(&self, member: HostId, old: Contribution, new: Contribution) -> bool {
        let shrank =
            |was: Option<u64>, is: Option<u64>| was.is_some_and(|was| is.is_none_or(|is| is < was));
        // The longest may fall as far as the second, which is exact, and stay the longest.
        let span_past_second = (self.of == Some(member)
            && new.span_ns.is_none_or(|span| span < self.second))
            || self.second_of == Some(member);
        let relay_past_second = self.relay_of == Some(member)
            && new.relay_due_ns.is_none_or(|due| due < self.relay_second);
        (span_past_second && shrank(old.span_ns, new.span_ns))
            || (relay_past_second && shrank(old.relay_due_ns, new.relay_due_ns))
    }

    /// Takes `member`'s span, at least what it was where it held one of the two longest.
    fn raise_span(&mut self, member: HostId, span: u64) {
        if self.of == Some(member) {
            self.first = span;
        } else if self.of.is_none() || span > self.first {
            if self.of.is_some() {
                self.second = self.first;
                self.second_of = self.of;
            }
            self.first = span;
            self.of = Some(member);
        } else if self.second_of == Some(member) || self.second_of.is_none() || span > self.second {
            self.second = span;
            self.second_of = Some(member);
        }
    }

    /// Takes `member`'s relay due, at least the second's bound where it was the slowest.
    fn raise_relay(&mut self, member: HostId, due: u64) {
        if self.relay_of == Some(member) {
            self.relay = due;
        } else if self.relay_of.is_none() || due > self.relay {
            if self.relay_of.is_some() {
                self.relay_second = self.relay_second.max(self.relay);
            }
            self.relay = due;
            self.relay_of = Some(member);
        } else {
            self.relay_second = self.relay_second.max(due);
        }
    }

    /// The longest span over every pair but `member`'s.
    fn without(&self, member: HostId) -> u64 {
        if self.of == Some(member) {
            self.second
        } else {
            self.first
        }
    }
}

/// The most `G` can be without changing the span of `peer`'s timer verdict ([`timer_verdict`]): its
/// variance term is `max(floor, K·RTTVAR)`, the floor `G` or the pool's margin where larger, and its
/// timeout at least RFC 6298's second while the pool has no verdict, so `G` decides nothing at or
/// under the larger of the others. Before any round trip, the initial second, which no `G` changes.
fn timer_g_limit(peer: &Peer, pooled: Option<Verdict>) -> u64 {
    let Some(smoothed) = peer.rto.smoothed_ns else {
        return u64::MAX;
    };
    let variation = peer.rto.variation_ns.saturating_mul(VARIATION_MULTIPLE);
    match pooled {
        Some(pooled) => variation.max(nanos(pooled.margin)),
        None => variation.max(MINIMUM_RTO_NS.saturating_sub(smoothed)),
    }
}

/// When a relay's response to a ping-request is due, from the request: this member's span for the
/// relay ([`judging`]), which bounds the request's way there and the response's way back, plus the
/// bound the relay stated for responding ([`Ack::relay_within_ns`]). `None` for a relay that has
/// stated none.
fn relay_due_ns(
    relay: &Peer,
    pooled: Option<Verdict>,
    loss: f64,
    granularity_ns: u64,
) -> Option<u64> {
    let within = relay.relay_within_ns?;
    Some(
        judging(relay, pooled, loss, granularity_ns)
            .0
            .span_ns()
            .saturating_add(within),
    )
}

/// What judges a pair's probes now ([`PeerReport::judge`]): the one rule [`Detector::verdict`]
/// reports and a probe is judged by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Judge {
    /// The pair's own retransmission timer (RFC 6298), which promises no bound: until an estimator
    /// judges it.
    #[default]
    Timer,
    /// The pair's own estimator's verdict.
    Own,
    /// The member's pooled verdict.
    Pool,
}

/// A pair's retransmission timer (RFC 6298) as its member measured it ([`PeerReport::timer`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimerReport {
    /// The timeout now, backed off where it expired since its latest round trip.
    pub timeout: Duration,
    /// Round trips taken.
    pub samples: u64,
    /// Of them, answers that came after their probe's period had ended.
    pub late: u64,
    /// The longest taken.
    pub longest: Duration,
}

/// What a member has done and promised about one peer, for its owner and its tests.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PeerReport {
    /// What judges the pair's probes now.
    pub judge: Judge,
    /// Whether the pair's own estimator configures its probes (rather than the pool's, or none).
    pub configured: bool,
    /// Suspicions this member's own probes started.
    pub suspicions: u64,
    /// Theorem 7's allowance for them: `Σβ` over every judged probe, the expected number of
    /// suspicions of the peer were it alive throughout.
    pub suspicion_allowance: f64,
    /// Times this member condemned the peer by its own probes.
    pub condemnations: u64,
    /// The allowance for condemnations of a live peer: over every judged probe, the bound on it
    /// and the probe before both going unanswered.
    pub condemnation_allowance: f64,
    /// From the peer's last answer to its condemnation, on this member's clock.
    pub condemned_after: Option<Duration>,
    /// The detection bound the member stated when it condemned: [`Detector::detection_bound`]
    /// plus the wait it measured for an answer from another member.
    pub condemned_within: Option<Duration>,
    /// When the peer last answered one of this member's probes, on the caller's clock.
    pub last_answer_ns: Option<u64>,
    /// When the probe that told the peer it was suspected went unanswered, on the caller's clock:
    /// from then the condemnation waits on an answer from another member, which nothing bounds in
    /// advance. Kept once the peer is dead; cleared when it is alive again.
    pub pending_since_ns: Option<u64>,
    /// Whether the pair's round trips do not feed the pool: its latest is past RFC 6298's timeout of
    /// the pool's round trip, another path than the pool's.
    pub pool_misfit: bool,
    /// Why the pair's own estimator last refused to configure, until it configures.
    pub refused: Option<Refusal>,
    /// The pair's retransmission timer (RFC 6298) over its probes' round trips.
    pub timer: TimerReport,
    /// How long after a ping-request reaches the peer it responds, as its latest acknowledgement
    /// stated ([`Ack::relay_within_ns`]); `None` before one, while the peer is asked to relay
    /// nothing.
    pub relay_within: Option<Duration>,
}

/// The relay stage as the member bounds it ([`Detector::relays`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayReport {
    /// The latest any relay this member could ask has its response due, from the request: its span
    /// for the relay plus the bound the relay stated ([`Ack::relay_within_ns`]). `None` while no
    /// alive peer has stated one, and an unanswered probe asks no relay.
    pub due: Option<Duration>,
    /// The peers that have stated a relay bound.
    pub stated: usize,
}

/// The member's pooled verdict and its evidence ([`Detector::pool`]): what judges every pair that
/// has no verdict of its own and that the pool's span covers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PoolReport {
    /// The verdict, once the pool first configured; a later refusal leaves it in force.
    pub verdict: Option<Verdict>,
    /// The round trips the pool took.
    pub samples: u64,
    /// Why the pool's latest configuration refused, until one succeeds.
    pub refused: Option<Refusal>,
}

/// Probes of one peer that may still be answered: the one that suspected it, the one that told it,
/// and the one more an extension can grant (at most one base window of one probe,
/// [`crate::extension`]). An older probe's acknowledgement is past every deadline that could use
/// it and is not kept for. Public so an owner that relays probes for its peers can bound what it
/// keeps by it: an asker waits on no more relayed answers about one target than its detector keeps
/// records for (slates' member plane keeps that many relayed probes per target and asker).
pub const OUTSTANDING: usize = 3;

/// A probe sent and not yet answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sent {
    seq: u64,
    nonce: u64,
    at_ns: u64,
}

/// A stream of round trips and the verdict configured from it: one peer's, or the pool's.
#[derive(Debug, Default)]
struct Stream {
    /// Boxed: the estimator's Allan levels are a kilobyte, and a peer's other fields are read
    /// every period, so they stay small and together (the allocation is made with the ring's).
    estimator: Option<Box<LinkEstimator>>,
    /// The sequence number the estimator was anchored at.
    anchor: u64,
    /// The granularity the estimator was last given, nanoseconds.
    granularity_ns: u64,
    verdict: Option<Verdict>,
    /// Round trips taken.
    samples: u64,
    /// Round trips taken at the last configuration, and the window then: the verdict is renewed
    /// once a window's worth more have come (Chen et al.'s adaptive detector, §6).
    configured_at: u64,
    renewal: u64,
    /// Why the latest configuration refused, until one succeeds: what tells a stream that has its
    /// samples and still promises no detector from one starved of them.
    refused: Option<Refusal>,
}

impl Stream {
    /// Takes the round trip of heartbeat `seq`, building the estimator at the stream's interval
    /// on the first.
    fn take(&mut self, seq: u64, rtt: u64, granularity: Duration, interval: Duration) {
        if self.estimator.is_none() {
            self.anchor = seq;
            self.granularity_ns = nanos(granularity);
            self.estimator =
                LinkEstimator::new(interval, granularity, Some(Schedule { seq, at_ns: 0 }))
                    .ok()
                    .map(Box::new);
        }
        let Some(estimator) = self.estimator.as_mut() else {
            return;
        };
        if nanos(granularity) != self.granularity_ns {
            self.granularity_ns = nanos(granularity);
            estimator.set_granularity(granularity);
        }
        // A sample from before the anchor, or past what a window can sum, is not taken.
        if feed(estimator, seq, self.anchor, rtt).is_ok() {
            self.samples = self.samples.saturating_add(1);
        }
    }

    /// Whether the verdict is due: never configured, or a window's worth of samples since.
    fn due(&self) -> bool {
        self.verdict.is_none() || self.samples.saturating_sub(self.configured_at) >= self.renewal
    }

    /// Configures the verdict from the estimates as they stand ([`verdict`]). A refusal leaves the
    /// verdict in force, as `LinkEstimator::configure` leaves its margin: a stall can make `τ_int`
    /// unmeasured again, and a probe that went unjudged then would suspect nobody.
    fn configure(
        &mut self,
        mtbf: Option<Duration>,
        granularity: Duration,
        interval: Duration,
        condemnation_cost: Duration,
    ) {
        let Some(estimator) = self.estimator.as_ref() else {
            return;
        };
        self.configured_at = self.samples;
        self.renewal = estimator.estimates().window.length;
        match verdict(estimator, mtbf, granularity, interval, condemnation_cost) {
            Ok(renewed) => {
                self.verdict = Some(renewed);
                self.refused = None;
            }
            Err(refusal) => self.refused = Some(refusal),
        }
    }
}

/// What this member holds about one peer.
#[derive(Debug)]
struct Peer {
    stream: Stream,
    /// Probes sent to the peer.
    sent: u64,
    outstanding: [Option<Sent>; OUTSTANDING],
    /// The probes sent when the current suspicion took hold: a probe numbered from it on told the
    /// peer (it carried the suspicion).
    suspected_from: Option<u64>,
    told_missed: u32,
    /// When the probe that told the peer went unanswered: its condemnation waits on an answer
    /// from another member.
    pending_since: Option<u64>,
    /// That probe's nonce: an answer to it or to a later probe, however late and by whatever path,
    /// shows the peer alive after it and takes the pending condemnation back.
    pending_nonce: Option<u64>,
    /// How long after a ping-request reaches the peer it responds, as its latest acknowledgement
    /// stated ([`Ack::relay_within_ns`]): with this member's span for the peer, when a relay stage
    /// that asks it has the peer's response due ([`Detector::request_indirect`]). A peer that has
    /// stated none is not asked.
    relay_within_ns: Option<u64>,
    /// The previous judged probe's bound.
    last_mistake: Option<f64>,
    last_answer_ns: Option<u64>,
    /// The pair's retransmission timer (RFC 6298) over its round trips: what judges its probes until
    /// an estimator does ([`timer_verdict`]). It backs off per pair (5.5), so a far peer among near
    /// ones is waited for long enough to be answered; a far peer judged by a deadline drawn from the
    /// near round trips was condemned again and again while alive, every answer arriving after its
    /// record was gone (slates, 2026-10-07: a 2 ms deadline on a 200 ms path, 76 to 115 condemnations
    /// a pair in 30 s).
    rto: Rto,
    report: PeerReport,
    /// The pair's latest measured round trip: its latest answered probe's, however late, a reused
    /// record's bound, else its handshake's. What decides whether the pool's span covers the pair
    /// ([`covered`]) and whether its round trips feed the pool ([`far`]).
    path_rtt_ns: Option<u64>,
    /// The latest probe whose record a later probe wrote over while it was still unanswered: an
    /// answer that finds no record is for it or for one sent before it, so its round trip is known
    /// exactly or bounded below ([`Peer::reused_round_trip`]).
    reused: Option<Sent>,
    /// What the pair adds to the member's extremes, as it was last judged ([`Detector::respan`]).
    contribution: Contribution,
    /// The peer's liveness in the view, kept as each change is adopted ([`Detector::adopted`]), so a
    /// judgement reads it without a lookup.
    liveness: Option<Liveness>,
}

impl Peer {
    fn new() -> Self {
        Self {
            stream: Stream::default(),
            sent: 0,
            outstanding: [None; OUTSTANDING],
            suspected_from: None,
            told_missed: 0,
            pending_since: None,
            pending_nonce: None,
            relay_within_ns: None,
            last_mistake: None,
            last_answer_ns: None,
            rto: Rto::default(),
            report: PeerReport::default(),
            path_rtt_ns: None,
            reused: None,
            contribution: Contribution::default(),
            liveness: None,
        }
    }

    /// The outstanding probe `nonce`, taken.
    fn take(&mut self, nonce: u64) -> Option<Sent> {
        self.outstanding
            .iter_mut()
            .find(|slot| slot.is_some_and(|sent| sent.nonce == nonce))
            .and_then(Option::take)
    }

    /// Records a probe sent, over the oldest still outstanding; that one, if still unanswered, is
    /// kept as the latest reused record.
    fn send(&mut self, sent: Sent) {
        let slots = u64::try_from(OUTSTANDING).unwrap_or(1);
        let at = usize::try_from(sent.seq.checked_rem(slots).unwrap_or(0)).unwrap_or(0);
        if let Some(slot) = self.outstanding.get_mut(at)
            && let Some(written_over) = slot.replace(sent)
        {
            self.reused = Some(written_over);
        }
    }

    /// An answer to `nonce` at `at_ns` whose probe's record later probes already reused: it came back
    /// after later probes of the pair. Its round trip, exact or a lower bound
    /// ([`Peer::reused_round_trip`]), is the pair's latest, so the pool judges a pair whose answers
    /// all come after their records are gone only once its span covers them, and the latest reused
    /// record's exact one is its timer's ([`Detector::on_ack`]). An answer no reuse explains (an owner's own ping on the same plane, its nonce
    /// from a range the member's probes never reach; an expired relay record's) is evidence of life
    /// only, which [`Detector::on_ack`] has taken: before 2026-10-09 it marked the pair a misfit for
    /// good, and an owner's lease renewals kept a pool from ever being fed (slates, a warm-restarted
    /// fleet, 3 of 11 runs). The round trip and whether it is exact.
    fn answered_unrecorded(&mut self, nonce: u64, at_ns: u64) -> Option<(u64, bool)> {
        if self.stream.verdict.is_some() {
            return None;
        }
        let (round_trip, exact) = self.reused_round_trip(nonce, at_ns)?;
        self.path_rtt_ns = Some(if exact {
            round_trip
        } else {
            self.path_rtt_ns
                .map_or(round_trip, |path| path.max(round_trip))
        });
        Some((round_trip, exact))
    }

    /// The round trip of an answer to `nonce` at `at_ns` that found no record, and whether it is
    /// exact: exact when it answers the latest reused record; a lower bound when it answers an
    /// earlier one, which was sent before that record (nonces only increase); `None` for a nonce
    /// after it, which no reuse explains.
    fn reused_round_trip(&self, nonce: u64, at_ns: u64) -> Option<(u64, bool)> {
        let reused = self.reused?;
        (nonce <= reused.nonce).then(|| (at_ns.saturating_sub(reused.at_ns), nonce == reused.nonce))
    }

    fn clear_pending(&mut self) {
        self.told_missed = 0;
        self.pending_since = None;
        self.pending_nonce = None;
        self.report.pending_since_ns = None;
    }

    /// An answer from the peer, direct or relayed, to this member's probe `nonce`, below `next`, the
    /// member's next probe nonce (an owner's own ping answered on the plane, or a ping the member
    /// relayed for another, is no probe of its own). An answer to the told probe or a later one shows
    /// the peer alive after the probe whose miss made its condemnation pending, which goes: the
    /// condemnation waited on an answer from another member, and the peer's own came first.
    fn answered_since_told(&mut self, nonce: u64, next: u64) {
        if self
            .pending_nonce
            .is_some_and(|told| nonce >= told && nonce < next)
        {
            self.clear_pending();
        }
    }

    fn clear_suspicion(&mut self) {
        self.suspected_from = None;
        self.clear_pending();
    }
}

/// The period's probe.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Probe {
    target: HostId,
    nonce: u64,
    seq: u64,
    sent_ns: u64,
    answered: bool,
    verdict: Verdict,
    /// When the relays' responses are due, once relays were asked: the slowest relay's span from
    /// this member plus the bound it stated ([`Detector::request_indirect`]).
    indirect_until: Option<u64>,
    /// Whether the verdict is the pair's timer's ([`timer_verdict`]): judged if it goes unanswered,
    /// but its period ends at its answer. Waiting out a timer's deadline on an answered probe
    /// stretched the member's periods past the interval its pairs' estimators were built at, and pairs
    /// near and far stopped configuring (33 of 40 seeds, 2026-10-07).
    provisional: bool,
    /// A timer-judged probe's expected arrival, its pair's smoothed round trip past its send, until
    /// the member has woken there: a wake that measures the member's lateness `G`. Its period ends
    /// at its answer, so without it no wake of a member whose answers all came before their
    /// deadlines was ever reached, `G` was never measured, no estimator took a sample, and none
    /// configured.
    expected: Option<u64>,
    /// The earliest its period may end: for a probe an estimator judges, sent plus the member's
    /// period floor ([`Detector::period_floor_ns`]); for a timer's, sent. Its deadlines are
    /// unchanged; only an end that would come sooner waits for this.
    rests_until: u64,
}

impl Probe {
    fn due_ns(&self) -> u64 {
        self.sent_ns.saturating_add(self.verdict.span_ns())
    }
}

/// A death the view adopted: the member, the incarnation it died at, which names the death (a
/// member leaves `Dead` only at a higher incarnation, so dies again only at one), and when the first
/// poll after it saw it, on the caller's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Death {
    member: HostId,
    incarnation: u64,
    at_ns: Option<u64>,
}

impl Death {
    /// Whether the view still holds the member dead at this death's incarnation.
    fn held(&self, membership: &Membership) -> bool {
        membership.state(self.member)
            == Some(MemberState {
                liveness: Liveness::Dead,
                incarnation: self.incarnation,
            })
    }
}

/// The member's periods: count, total and longest, nanoseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Periods {
    count: u64,
    total: u128,
    longest: u64,
}

impl Periods {
    fn add(&mut self, length: u64) {
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(u128::from(length));
        self.longest = self.longest.max(length);
    }

    fn mean_ns(&self) -> Option<u64> {
        self.total
            .checked_div(u128::from(self.count))
            .and_then(|mean| u64::try_from(mean).ok())
            .filter(|mean| *mean > 0)
    }
}

/// `duration` in nanoseconds, saturating at `u64::MAX` (584 years).
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// SWIM §4.1's dissemination budget for `members`: an update piggybacked for `λ·ln n` periods
/// leaves at most `n^{−((2−4/n)λ−2)}` members uninfected in expectation, which is below one member
/// once `λ > n/(n−2)`. The budget is the least whole count past `n·ln n/(n−2)`. With two members
/// or fewer every message reaches the only other one, and one transmission is all there is to
/// send. The logarithm is the fixed-point one ([`crate::fixed`]), so every host computes the same
/// budget.
pub fn gossip_transmits(members: usize) -> u32 {
    let n = u64::try_from(members).unwrap_or(u64::MAX);
    if n <= 2 {
        return 1;
    }
    let fraction = f64::from(1u32 << crate::fixed::FRACTION_BITS);
    // u64 → f64 rounds only past 2⁵³ members.
    let ln = crate::fixed::log2_fixed(n) as f64 / fraction * std::f64::consts::LN_2;
    let rounds = n as f64 * ln / (n.saturating_sub(2)) as f64;
    // `rounds` is at most about 45 (ln 2⁶⁴ with n/(n−2) near one), so the count ends quickly.
    let mut budget = 1u32;
    while f64::from(budget) <= rounds && budget < u32::MAX {
        budget = budget.saturating_add(1);
    }
    budget
}

/// The relays an indirect probe asks: the fewest whose paths together fail no more often than the
/// direct probe did. A relayed probe is two round trips, so with per-probe loss `p` one relay fails
/// with `1 − (1 − p)²`; `k` relays all fail with that to the `k`th, and the retry is at least as
/// reliable as the try it backs up once that is at most `p`. Bounded by the relays there are.
fn relay_count(loss: f64, available: usize) -> usize {
    let through = 1.0 - (1.0 - loss) * (1.0 - loss);
    let mut all_fail = through;
    let mut count = 1usize;
    while all_fail > loss && count < available {
        all_fail *= through;
        count = count.saturating_add(1);
    }
    count.min(available)
}

/// A push of this member's view to `to`, chunk by chunk ([`Detector::sync_into`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Push {
    to: HostId,
    /// The last member sent; `None` before the first chunk.
    after: Option<HostId>,
    /// Whether the next chunk asks the partner's view in return: the first chunk of the answer to
    /// an opening whose digest differed.
    pull: bool,
}

/// A message of anti-entropy to send ([`Detector::sync_into`]): to whom, whether it asks the
/// receiver's view in return, and the digest of this member's view. The owner sends it as a
/// [`SwimMessage::Sync`](crate::codec::SwimMessage::Sync), carrying the members the call put in
/// its batch: none in an exchange's opening.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewChunk {
    /// The member to send the chunk to.
    pub to: HostId,
    /// Whether the chunk asks the receiver's view in return.
    pub pull: bool,
    /// The digest of this member's view ([`Membership::digest`]).
    pub digest: u64,
}

/// Anti-entropy (Demers et al. 1987, §1.3 and §1.5): once a dissemination window, an exchange with
/// one member, the partners in a shuffled cycle of the members this one holds alive. It opens with
/// the view's digest; only views whose digests differ are pushed, both ways.
#[derive(Debug, Default)]
struct Exchanges {
    /// When this member last began an exchange.
    began_ns: Option<u64>,
    /// The cycle of partners, rebuilt and shuffled when it runs out, like the probe order.
    partners: Vec<HostId>,
    cursor: usize,
    /// The partner of the exchange this member began, while its opening is still to send.
    opening: Option<HostId>,
    /// The push of its view it owes: to an opening whose digest differed from its own, asking the
    /// opener's view, or to a push that asked its own.
    answer: Option<Push>,
    /// Pulls refused because an answer to another member was owed.
    refused: u64,
}

/// The failure detector for one node: its [`Membership`] view, its probe rotation, the period's
/// probe, and per peer the estimator and verdict that time its probes.
pub struct Detector {
    /// The owner's detection budget, nanoseconds: the longest it may take this member to declare a
    /// dead member. What sets the period floor ([`Detector::period_floor_ns`]).
    detection_budget_ns: u64,
    /// What the owner's response to a death costs it ([`Detector::set_condemnation_cost`]): the
    /// price of a false condemnation in every verdict's margin.
    condemnation_cost: Duration,
    membership: Membership,
    local: HostId,
    order: Vec<HostId>,
    cursor: usize,
    probe: Option<Probe>,
    peers: BTreeMap<HostId, Peer>,
    /// Every round trip the member measured, to anyone: the judge of a pair that cannot configure
    /// yet. Its sequence numbers are the member's probe nonces, so a probe never answered is a
    /// loss.
    pool: Stream,
    gossip: Gossip,
    transmits: u32,
    shuffler: RandomizedOrder,
    coordinates: CoordinateEngine,
    peer_coordinates: BTreeMap<HostId, NetworkCoordinate>,
    /// Probes this member has sent: the next probe's nonce.
    nonce: u64,
    /// Probes relayed for others, counted down from the top of the nonce space so they never meet
    /// the member's own.
    relayed: u64,
    /// The wakes asked of the caller and how late each came: `G` and the latest lateness.
    wakes: Wakes,
    /// The resolution of the clock the owner reads, nanoseconds: `G`'s least value
    /// ([`Detector::granularity_ns`]).
    resolution_ns: u64,
    /// `G` in nanoseconds as the latest measured wake left it ([`Detector::granularity_ns`]): read
    /// at every judgement, and changed only where a wake is measured.
    g_ns: u64,
    /// Changes of the pool's verdict, counted: what the extremes compare to know the pool's verdict
    /// changed since they were judged ([`Detector::refresh_extremes`]).
    pool_epoch: u64,
    /// The pool's mean round trip, nanoseconds, and the probes it counts lost and received, as its
    /// latest sample left them ([`Detector::pool_round_trip`], [`Detector::probe_loss`]): read at
    /// every answer and period, and changed only where the pool takes a sample.
    pool_mean_ns: Option<u64>,
    pool_counts: (u64, u64),
    /// Round trips measured before this member had a wake and a period measured, which no
    /// estimator took.
    unmeasured: u64,
    /// The least round trip measured exactly, to anyone: an answer to a probe still recorded, an
    /// answer to the latest reused record, or a handshake's. The pool's round trip before the pool
    /// holds a sample ([`Detector::pool_round_trip`]).
    least_rtt_ns: Option<u64>,
    /// The extremes over the peers' contributions, kept as they change ([`Extremes`]): the longest
    /// deadline for a probe of its own, with any one pair left out, which bounds its deadline for a
    /// ping it relays for that member ([`Detector::on_ping_req`]), and the slowest relay's due.
    extremes: Extremes,
    /// The peers a change of the pool's verdict or of `G` judges again, held across periods so it
    /// allocates nothing once grown.
    rejudge: Vec<HostId>,
    /// The longest an unanswered probe of a pair this member probes runs, its direct span and its
    /// relay stage, as the latest period began, nanoseconds ([`Detector::period_bound`]).
    longest_unanswered: u64,
    periods: Periods,
    exposure: Exposure,
    /// Protocol periods run: the clock an extension's rate limit counts in.
    period: u64,
    /// The extensions granted to each suspected member (mantle note 32 S13).
    extensions: BTreeMap<HostId, ExtensionTracker>,
    /// The suspects a condemnation visits, held across periods so it allocates nothing once grown.
    aging: Vec<(HostId, u64)>,
    /// The relays an indirect probe ranks, held for the same reason.
    relays: Vec<HostId>,
    /// The relays the period's probe asked that have not responded, an answer or a nack: its relay
    /// stage ends when none is left, or once the slowest could have responded
    /// ([`Detector::request_indirect`]). Held across periods so it allocates nothing once grown.
    awaiting: Vec<HostId>,
    /// The deaths the view adopted, oldest first: the order their records are forgotten in. A
    /// record the member has since left (a refutation, a newer death) is skipped when reached.
    deaths: VecDeque<Death>,
    /// The latest deaths counted as failures, each a member and the incarnation it died at, at most
    /// as many as the view holds members. A record forgotten can come back: a member the death had
    /// not reached yet passes on its older state of the dead member, which is adopted as a
    /// newcomer's, condemned again and forgotten again (`docs/research/swim.md`, what is left of
    /// the window's risk); it is the same death, counted once.
    counted: VecDeque<(HostId, u64)>,
    /// The latest time the caller gave, nanoseconds.
    clock_ns: u64,
    /// The most members the view has held at once besides this one: no round has been larger.
    most_watched: u64,
    /// The largest dissemination budget any report of this member has been sent with.
    most_transmits: u32,
    /// Updates refused at the view's bound.
    refused: u64,
    /// The anti-entropy exchanges.
    exchanges: Exchanges,
    /// What the latest poll found, in order. A poll ends at most one probe's period, which finds
    /// one suspicion or pending condemnation or, answered, condemns every other member with one
    /// pending: fewer findings than the view holds members, its capacity from the start.
    findings: Vec<Finding>,
}

/// A deterministic pseudo-random order over the members to probe (SWIM §4.3): each round probes a
/// fresh shuffled permutation, so every member is probed once a round and successive probes of one
/// member are at most `2m − 1` periods apart. Seeded from the node id, so a simulation replays.
struct RandomizedOrder {
    state: u64,
}

/// Format: the golden-ratio odd constant (2^64 / φ), the standard seed mixer, so distinct node ids seed
/// visibly different sequences.
const SEED_MIXER: u64 = 0x9E37_79B9_7F4A_7C15;
/// Format: Marsaglia's xorshift64 shift triple (`Xorshift RNGs`, 2003) — the three shifts of the
/// full-period 64-bit generator.
const XORSHIFT_TRIPLE: [u32; 3] = [13, 7, 17];

impl RandomizedOrder {
    /// A generator seeded from `local`, never zero (xorshift stays at zero forever from a zero seed).
    fn seeded(local: HostId) -> RandomizedOrder {
        RandomizedOrder {
            state: (local.0 ^ SEED_MIXER) | 1,
        }
    }

    /// The next pseudo-random word (xorshift64).
    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << XORSHIFT_TRIPLE[0];
        x ^= x >> XORSHIFT_TRIPLE[1];
        x ^= x << XORSHIFT_TRIPLE[2];
        self.state = x;
        x
    }

    /// Shuffles `items` in place with a Fisher–Yates pass driven by the generator.
    fn shuffle(&mut self, items: &mut [HostId]) {
        let len = items.len();
        for index in (1..len).rev() {
            let span = u64::try_from(index).unwrap_or(0).saturating_add(1);
            let pick = usize::try_from(self.next().checked_rem(span).unwrap_or(0)).unwrap_or(0);
            items.swap(index, pick);
        }
    }
}

/// Where the period's probe stands.
enum Stage {
    /// Waiting for an answer or a deadline.
    Wait,
    /// The direct deadline passed unanswered: ask relays.
    Indirect,
    /// The period is over.
    Over,
}

impl Detector {
    /// A detector for `local`, with the fleet's failure history so far (`history`, the node time it
    /// has run and the failures it has had; [`Exposure::new`] for a fleet with none), whose view holds
    /// at most `members`, itself included: how many hosts the owner's placement says this node can
    /// know. `resolution` is that of the clock the owner reads `now` and the acknowledgements'
    /// stamps on, the least step its readings take: a wake read exactly on time was late by less
    /// than it, so it bounds `G` from below (`hyper_timing::Lateness`). hyper-tokio's
    /// `Clock::resolution` states the host's monotonic clock's; a simulation's stamps are whole
    /// nanoseconds. `detection_budget` is the longest the owner may wait for this member to declare
    /// a dead member, its requirement (Chen, Toueg and Aguilera's `T_D^U`): a judged period lasts at
    /// least the share of it the detection bound spends on one period
    /// (`docs/timing.md` §2.7), so an idle member probes at the pace its owner needs and not
    /// at its round trip's. Zero leaves every period at what its probe needs.
    pub fn new(
        local: HostId,
        history: Exposure,
        members: NonZeroUsize,
        resolution: Duration,
        detection_budget: Duration,
    ) -> Detector {
        Detector {
            detection_budget_ns: nanos(detection_budget),
            condemnation_cost: Duration::ZERO,
            membership: Membership::new(local, members),
            local,
            order: Vec::new(),
            cursor: 0,
            probe: None,
            peers: BTreeMap::new(),
            pool: Stream::default(),
            gossip: Gossip::default(),
            transmits: 1,
            shuffler: RandomizedOrder::seeded(local),
            coordinates: CoordinateEngine::new(local.0),
            peer_coordinates: BTreeMap::new(),
            nonce: 0,
            relayed: u64::MAX,
            wakes: Wakes::new(resolution),
            resolution_ns: nanos(resolution),
            g_ns: nanos(resolution),
            pool_epoch: 0,
            pool_mean_ns: None,
            pool_counts: (0, 0),
            unmeasured: 0,
            least_rtt_ns: None,
            extremes: Extremes::default(),
            rejudge: Vec::new(),
            longest_unanswered: 0,
            periods: Periods::default(),
            exposure: history,
            period: 0,
            extensions: BTreeMap::new(),
            aging: Vec::new(),
            relays: Vec::new(),
            awaiting: Vec::new(),
            deaths: VecDeque::new(),
            counted: VecDeque::with_capacity(members.get()),
            clock_ns: 0,
            most_watched: 0,
            most_transmits: 1,
            refused: 0,
            // The cycle never holds more than the view: sized once, so no exchange grows it.
            exchanges: Exchanges {
                partners: Vec::with_capacity(members.get()),
                ..Exchanges::default()
            },
            findings: Vec::with_capacity(members.get()),
        }
    }

    /// Advances the detector to `now_ns` (the caller's monotonic clock): ends the period when its
    /// probe is answered past its deadline, or unanswered past the indirect probe's; asks relays
    /// when the direct deadline passes unanswered (`requests`, replaced); and starts the next
    /// period, returning its [`Ping`] for the caller to send now. Called at every
    /// [`wake`](Detector::wake) and after every message the caller feeds in. What its end of a
    /// period found is in [`findings`](Detector::findings) until the next poll.
    pub fn poll(&mut self, now_ns: u64, requests: &mut Vec<PingReq>) -> Option<Ping> {
        requests.clear();
        self.findings.clear();
        self.wakes.woke(now_ns);
        self.g_ns = self.granularity().map_or(self.resolution_ns, nanos);
        self.clock_ns = self.clock_ns.max(now_ns);
        if let Some(probe) = self.probe.as_mut()
            && probe.expected.is_some_and(|expected| now_ns >= expected)
        {
            probe.expected = None;
        }
        self.stamp_deaths(now_ns);
        self.forget_expired(now_ns);
        self.begin_exchange(now_ns);
        let ping = match self.stage(now_ns) {
            Stage::Wait => None,
            Stage::Indirect => {
                self.request_indirect(now_ns, requests);
                if requests.is_empty() {
                    self.next_period(now_ns)
                } else {
                    None
                }
            }
            Stage::Over => self.next_period(now_ns),
        };
        self.wakes.ask(self.wake());
        ping
    }

    /// When to [`poll`](Detector::poll) next, on the caller's clock: the probe's deadline, or the
    /// indirect probe's once asked, never before the period floor's end unless the deadline asks
    /// relays. `None` before the first poll.
    pub fn wake(&self) -> Option<u64> {
        let probe = self.probe?;
        // A timer-judged probe still unanswered wakes at its expected arrival first, measuring `G`.
        if !probe.answered
            && let Some(expected) = probe.expected
        {
            return Some(expected.min(probe.due_ns()));
        }
        // The direct deadline of a probe still unanswered asks relays: kept as it is.
        if !probe.answered && probe.indirect_until.is_none() {
            return Some(probe.due_ns());
        }
        let ends = if probe.answered {
            (!probe.provisional).then(|| probe.due_ns())
        } else if self.awaiting.is_empty() {
            // Every relay asked has nacked: the relay stage is over.
            None
        } else {
            probe.indirect_until
        };
        Some(ends.map_or(probe.rests_until, |at| at.max(probe.rests_until)))
    }

    /// Sets the owner's detection budget, [`Detector::new`]'s `detection_budget`, which an owner
    /// re-derives as its own measurements change (slates: its failover objective less the council's
    /// window for confirming a death). A period is floored by the budget in force as it starts: one
    /// resting now keeps the rest its start set, and the next is floored by the new budget.
    pub fn set_detection_budget(&mut self, budget: Duration) {
        self.detection_budget_ns = nanos(budget);
    }

    /// States what the owner's response to a death costs it, as the unavailability it causes:
    /// slates', a retirement and the takeovers it starts. A false condemnation costs that, so each
    /// verdict configured from now on charges it to every probe at the probe's bound, beside a
    /// false suspicion's refutation, and its margin is the wider for it; a verdict in
    /// force keeps its margin until it is renewed. Zero, the default, prices a false condemnation as
    /// a false suspicion: an owner that takes no action on a death.
    pub fn set_condemnation_cost(&mut self, cost: Duration) {
        self.condemnation_cost = cost;
    }

    /// The least a judged period of this member lasts, nanoseconds: the detection budget's share of
    /// one period. A dead member is declared within `2(2m − 1) + 1` periods of its last answer, `m`
    /// the members a round probes (`docs/timing.md` §2.7), so periods of at least
    /// `D / (2(2m − 1) + 1)` keep that within the owner's budget `D` while probing no faster than
    /// it needs. SWIM sets its protocol period for load and bounds it below by the round trip (Das
    /// et al. 2002, §3.1); here the round trip's deadline still bounds it below, and the budget
    /// sets the load. Without the floor an idle pair probed at its round trip: about 1,700 probes a
    /// second per member on loopback (slates, 2026-10-09).
    fn period_floor_ns(&self) -> u64 {
        let watched = u64::try_from(self.order.len()).unwrap_or(u64::MAX).max(1);
        let periods = watched
            .saturating_mul(2)
            .saturating_sub(1)
            .saturating_mul(2)
            .saturating_add(1);
        self.detection_budget_ns.checked_div(periods).unwrap_or(0)
    }

    fn stage(&self, now_ns: u64) -> Stage {
        match (self.probe, self.deadline_stage(now_ns)) {
            (Some(probe), Stage::Over) if now_ns < probe.rests_until => Stage::Wait,
            (_, stage) => stage,
        }
    }

    /// Where the period's probe stands by its deadlines alone, before the period floor.
    fn deadline_stage(&self, now_ns: u64) -> Stage {
        let Some(probe) = self.probe else {
            return Stage::Over;
        };
        let due = probe.due_ns();
        match (probe.answered, probe.indirect_until) {
            // A timer-judged probe's period ends at its answer.
            (true, _) if probe.provisional => Stage::Over,
            (true, _) | (false, None) if now_ns < due => Stage::Wait,
            (true, _) => Stage::Over,
            (false, None) => Stage::Indirect,
            // The relay stage waits while a relay asked has not responded, until its response is
            // due.
            (false, Some(until)) if now_ns < until && !self.awaiting.is_empty() => Stage::Wait,
            (false, Some(_)) => Stage::Over,
        }
    }

    /// Resolves the period's probe and starts the next.
    fn next_period(&mut self, now_ns: u64) -> Option<Ping> {
        if let Some(probe) = self.probe.take() {
            self.resolve(probe, now_ns);
        }
        self.period = self.period.saturating_add(1);
        let membership = &self.membership;
        self.extensions.retain(|host, _| {
            membership.state(*host).map(|state| state.liveness) == Some(Liveness::Suspect)
        });
        self.start(now_ns)
    }

    /// Starts a probe of the next member in the rotation at `now_ns`.
    fn start(&mut self, now_ns: u64) -> Option<Ping> {
        let target = self.next_target()?;
        let nonce = self.nonce;
        self.nonce = self.nonce.saturating_add(1);
        // The pool's verdict as it judges this probe: configured now if it is due, before the
        // handshake is compared with it. Comparing with the verdict held before configuring read none
        // on the probe that first configured the pool, and that probe was judged by the pool however
        // long the pair's handshake had measured its path (a near member condemned by a far one in 5
        // of 40 seeds, 2026-10-07).
        let pooled = self.pooled();
        let loss = self.probe_loss();
        let granularity_ns = self.granularity_ns();
        // The handshake that keyed the pair already measured its path: a round trip longer than the
        // pool's deadline is the same evidence a late answer gives, known before the first probe
        // ([`covered`]).
        let created = !self.peers.contains_key(&target);
        let peer = self.peers.entry(target).or_insert_with(Peer::new);
        let (verdict, provisional) = judging(peer, pooled, loss, granularity_ns);
        let peer_expected = peer.rto.smoothed_ns;
        let seq = peer.sent;
        peer.sent = peer.sent.saturating_add(1);
        peer.send(Sent {
            seq,
            nonce,
            at_ns: now_ns,
        });
        let probe = Probe {
            target,
            nonce,
            seq,
            sent_ns: now_ns,
            answered: false,
            verdict,
            indirect_until: None,
            provisional,
            expected: peer_expected
                .filter(|_| provisional)
                .map(|round_trip| now_ns.saturating_add(round_trip)),
            // Only a period an estimator judges rests: a timer-judged one ends at its answer, as the
            // samples its pair's estimator is still gathering need; stretched to the floor, a lone
            // peer stayed alive 9 s into its silence (slates, 2026-10-09).
            rests_until: if provisional {
                now_ns
            } else {
                now_ns.saturating_add(self.period_floor_ns())
            },
        };
        self.probe = Some(probe);
        // The longest an unanswered probe of a pair it probes now runs: the span of the verdict
        // judging it, and then its relay stage, until the slowest relay it could ask has its
        // response due ([`Detector::request_indirect`]).
        if created {
            self.respan(target);
        }
        self.refresh_extremes();
        self.longest_unanswered = self.extremes.first.saturating_add(self.extremes.relay);
        Some(Ping {
            to: target,
            nonce,
            due_ns: probe.due_ns(),
        })
    }

    /// What `peer` adds to this member's extremes now ([`Contribution`]): its pair's span where it
    /// is probed (alive or suspected), and where it may be asked to relay (alive, with a stated
    /// bound) when a request to it has its response due. The member's probe loss sizes the relays a
    /// probe asks and moves no span, so it is not read.
    fn contribution(
        &self,
        host: HostId,
        peer: &Peer,
        pooled: Option<Verdict>,
        granularity_ns: u64,
    ) -> Contribution {
        let liveness = peer.liveness;
        let probed = matches!(liveness, Some(Liveness::Alive | Liveness::Suspect));
        let within = peer
            .relay_within_ns
            .filter(|_| host != self.local && liveness == Some(Liveness::Alive));
        if !probed && within.is_none() {
            return Contribution::default();
        }
        let (verdict, timer) = judging(peer, pooled, 0.0, granularity_ns);
        let span = verdict.span_ns();
        Contribution {
            span_ns: probed.then_some(span),
            relay_due_ns: within.map(|within| span.saturating_add(within)),
            timer_g_ns: timer.then(|| timer_g_limit(peer, pooled)),
            follows_pool: peer.stream.verdict.is_none(),
        }
    }

    /// Judges `host`'s pair again after a change to what it is judged by: its answers, its timer,
    /// its stated relay bound or its liveness. Every such change calls this, and the extremes follow
    /// ([`Extremes::change`]).
    fn respan(&mut self, host: HostId) {
        let (pooled, granularity_ns) = (self.pool.verdict, self.granularity_ns());
        let Some(new) = self
            .peers
            .get(&host)
            .map(|peer| self.contribution(host, peer, pooled, granularity_ns))
        else {
            return;
        };
        let Some(peer) = self.peers.get_mut(&host) else {
            return;
        };
        let old = std::mem::replace(&mut peer.contribution, new);
        if old != new {
            self.extremes.change(host, old, new);
        }
    }

    /// `host`'s relay bound restated as `within_ns` and nothing else of its pair changed: its relay
    /// due follows from its span as last judged. A peer not yet counted as a relay is judged whole.
    fn restate(&mut self, host: HostId, within_ns: u64) {
        let Some(peer) = self.peers.get_mut(&host) else {
            return;
        };
        let old = peer.contribution;
        let (Some(span), Some(_)) = (old.span_ns, old.relay_due_ns) else {
            self.respan(host);
            return;
        };
        let new = Contribution {
            relay_due_ns: Some(span.saturating_add(within_ns)),
            ..old
        };
        peer.contribution = new;
        if old != new {
            self.extremes.change(host, old, new);
        }
    }

    /// Takes the extremes again from every peer's contribution, as last judged.
    fn rescan(&mut self) {
        let mut extremes = Extremes {
            g_ns: self.extremes.g_ns,
            pool_epoch: self.extremes.pool_epoch,
            ..Extremes::default()
        };
        for (host, peer) in &self.peers {
            extremes.change(*host, Contribution::default(), peer.contribution);
        }
        self.extremes = extremes;
    }

    /// Brings the extremes to the pool's verdict and `G` now, before they are read: a changed pool
    /// verdict judges again every pair that has none of its own, whose judgement it can change; a
    /// `G` past some timer's limit, every pair a timer judges. Stale extremes are taken again.
    fn refresh_extremes(&mut self) {
        let granularity_ns = self.granularity_ns();
        let pool_changed = self.pool_epoch != self.extremes.pool_epoch;
        let g_moved = granularity_ns != self.extremes.g_ns
            && granularity_ns.max(self.extremes.g_ns) > self.extremes.g_limit;
        if pool_changed || g_moved {
            let mut rejudge = std::mem::take(&mut self.rejudge);
            rejudge.clear();
            rejudge.extend(
                self.peers
                    .iter()
                    .filter(|(_, peer)| {
                        (pool_changed && peer.contribution.follows_pool)
                            || (g_moved && peer.contribution.timer_g_ns.is_some())
                    })
                    .map(|(host, _)| *host),
            );
            for &host in &rejudge {
                self.respan(host);
            }
            self.rejudge = rejudge;
            // A timer's limit only falls as contributions change: taken again with the rest.
            self.extremes.stale |= g_moved;
        }
        self.extremes.pool_epoch = self.pool_epoch;
        self.extremes.g_ns = granularity_ns;
        if self.extremes.stale {
            self.rescan();
        }
    }

    /// How long after a ping-request from `asker` reaches this member it responds, with the
    /// target's answer or a nack ([`Ack::relay_within_ns`]): its longest deadline for a probe of its
    /// own of any member but `asker`, which it is never asked about, as the latest period began,
    /// which bounds its deadline for a ping it relays for `asker` ([`Detector::on_ping_req`]); plus
    /// `G`, the mean lateness of its wakes, since it nacks at a wake and answers a ping as it
    /// arrives. Its stalls are not added: they hold its acknowledgements as long as its nacks, so a
    /// prober's span for it, measured from those acknowledgements, already covers them. Measured in
    /// the simulation, 16 seeds each: a bound over every pair stated a relay's span for the asker
    /// back to the asker, and with two near members and one 200 ms away the far member's detection
    /// bound was 4,585 ms, where without its own pair it is 3,147 ms (4,601 ms before relays stated
    /// bounds); and the latest lateness seen, in the place of `G`, states a stall for good: a member
    /// stopped until it was condemned stated up to 31 ms where `G` gives 4 ms, and the longest bound
    /// the others stated went from 606 to 725 ms.
    fn relay_within_ns(&self, asker: HostId) -> u64 {
        self.extremes
            .without(asker)
            .saturating_add(self.granularity_ns())
    }

    /// `G` in nanoseconds: the mean lateness of this member's wakes, never below the resolution of
    /// the clock its owner reads; before a wake is measured, that resolution, the least `G` can be.
    fn granularity_ns(&self) -> u64 {
        self.g_ns
    }

    /// The member's probe loss: Jeffreys' posterior mean over the probes its pool has seen,
    /// `(k + ½)/(n + 1)` for `k` of `n` lost (Jeffreys 1946; Brown, Cai and DasGupta 2001), the
    /// prior's half before it has seen any. A probe unanswered to anyone is a loss to the pool. It
    /// sizes the relays a timer-judged probe asks ([`relay_count`]).
    fn probe_loss(&self) -> f64 {
        let (lost, received) = self.pool_counts;
        // u64 → f64 rounds only past 2⁵³ probes.
        (lost as f64 + 0.5) / (lost as f64 + received as f64 + 1.0)
    }

    /// Fills `requests` with the relays to ask for the period's probe (SWIM's indirect probe), and
    /// sets when the relay stage ends. Each relay judges its own leg to the target, by its own
    /// deadline for it, and with no answer by then it nacks (Lifeguard's nack, Dadgar et al.
    /// §IV-A): the stage ends once every relay asked has nacked, or once the slowest has its
    /// response due, this member's span for it plus the bound it stated for its responses
    /// ([`relay_due_ns`]). Lifeguard's asker knows when a relay's nack is due because every member
    /// runs one configured probe timeout; deadlines measured per member are known to the member
    /// alone, so each states its bound on its acknowledgements. Two forms came first. The relay
    /// stage was timed by the slowest relay's span plus this member's span for the target, as if
    /// the relay's leg to the target were this member's: a target whose direct path was lost
    /// keeps the span it had then, and at load 100 relayed answers came past it, each a suspicion
    /// and then a condemnation of a live member (slates, 2026-10-10). Then by RFC 6298's timer over
    /// the relayed exchanges, cold at RFC 8961 (1)'s second: relays are asked only when a probe
    /// goes unanswered, so on a network that answers every probe the timer was never measured, and
    /// every period bound, and every window drawn from it, stayed a second long (no anti-entropy
    /// exchange began in 1,000 periods of 1 ms round trips). With no relay to ask, nothing is due,
    /// and the period ends at the direct deadline.
    fn request_indirect(&mut self, now_ns: u64, requests: &mut Vec<PingReq>) {
        let Some(mut probe) = self.probe else {
            return;
        };
        self.rank_relays(probe.target);
        let count = relay_count(probe.verdict.loss, self.relays.len());
        let (pooled, loss, granularity_ns) =
            (self.pool.verdict, self.probe_loss(), self.granularity_ns());
        let mut awaiting = std::mem::take(&mut self.awaiting);
        awaiting.clear();
        let mut slowest = 0u64;
        for &relay in self.relays.iter().take(count) {
            let Some(due) = self
                .peers
                .get(&relay)
                .and_then(|peer| relay_due_ns(peer, pooled, loss, granularity_ns))
            else {
                continue;
            };
            requests.push(PingReq {
                relay,
                target: probe.target,
                nonce: probe.nonce,
            });
            awaiting.push(relay);
            slowest = slowest.max(due);
        }
        self.awaiting = awaiting;
        if requests.is_empty() {
            return;
        }
        let until = now_ns.saturating_add(slowest);
        probe.indirect_until = Some(until);
        self.probe = Some(probe);
        // A relay that restated a longer bound since the period began lengthens this period past
        // what its start allowed: the bound counts the stage as set.
        self.longest_unanswered = self
            .longest_unanswered
            .max(until.saturating_sub(probe.sent_ns));
    }

    /// Ranks the alive peers other than `target` that have stated a relay bound as relays, nearest
    /// the target first.
    fn rank_relays(&mut self, target: HostId) {
        let mut relays = std::mem::take(&mut self.relays);
        relays.clear();
        relays.extend(self.membership.alive().filter(|host| {
            *host != self.local
                && *host != target
                && self
                    .peers
                    .get(host)
                    .is_some_and(|peer| peer.relay_within_ns.is_some())
        }));
        relays.sort_by(|a, b| {
            match (
                self.predicted_between(*a, target),
                self.predicted_between(*b, target),
            ) {
                (Some(x), Some(y)) => x.total_cmp(&y).then(a.0.cmp(&b.0)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.0.cmp(&b.0),
            }
        });
        self.relays = relays;
    }

    /// The period ends: its length is folded, the exposure grows, and an unanswered judged probe
    /// suspects its target or, when it had told the target, condemns it.
    fn resolve(&mut self, probe: Probe, now_ns: u64) {
        let length = now_ns.saturating_sub(probe.sent_ns);
        self.periods.add(length);
        let watched = u32::try_from(self.order.len()).unwrap_or(u32::MAX);
        self.exposure
            .on_exposure(Duration::from_nanos(length).saturating_mul(watched));
        let Some(peer) = self.peers.get_mut(&probe.target) else {
            return;
        };
        Self::account(peer, probe.verdict);
        if probe.answered {
            peer.clear_pending();
            self.condemn_pending(probe.target, probe.nonce, now_ns);
        } else {
            if probe.provisional {
                // The pair's timer expired with no answer: its next timeout doubles (RFC 6298
                // (5.5)), until an answer is measured.
                peer.rto.expired();
                self.respan(probe.target);
            }
            self.missed(probe, now_ns);
        }
    }

    /// Theorem 7's allowance for a judged probe: its bound for a suspicion, and for a
    /// condemnation the bound on it and the previous probe both missing, the lesser of the two
    /// (Fréchet's bound). Not their product: a pair's probes are a few periods apart, inside the
    /// stalls' correlation time the traces measured (20 to 250 ms, `docs/timing.md` §2.6), and a
    /// short history's `τ_int` of one has not yet seen a stall.
    fn account(peer: &mut Peer, verdict: Verdict) {
        let report = &mut peer.report;
        report.suspicion_allowance += verdict.mistake;
        if let Some(previous) = peer.last_mistake {
            report.condemnation_allowance += previous.min(verdict.mistake);
        }
        peer.last_mistake = Some(verdict.mistake);
    }

    /// A judged probe went unanswered: its target, alive, is suspected; suspected and told by it,
    /// its condemnation is pending. Each is found with the probe's evidence.
    fn missed(&mut self, probe: Probe, now_ns: u64) {
        let due_ns = probe.due_ns();
        let Some(state) = self.membership.state(probe.target) else {
            return;
        };
        let unanswered = Unanswered {
            target: probe.target,
            nonce: probe.nonce,
            sent_ns: probe.sent_ns,
            due_ns,
            relays_due_ns: probe.indirect_until,
            ended_ns: now_ns,
        };
        match state.liveness {
            Liveness::Alive => {
                if let Some(peer) = self.peers.get_mut(&probe.target) {
                    peer.report.suspicions = peer.report.suspicions.saturating_add(1);
                }
                // An update about a member the view holds is never refused.
                let _held = self.record(
                    probe.target,
                    MemberState {
                        liveness: Liveness::Suspect,
                        incarnation: state.incarnation,
                    },
                );
                self.findings.push(Finding::Suspected(unanswered));
            }
            Liveness::Suspect => {
                let granted = self
                    .extensions
                    .get(&probe.target)
                    .map_or(0, ExtensionTracker::total);
                if let Some(peer) = self.peers.get_mut(&probe.target)
                    && peer.suspected_from.is_some_and(|from| probe.seq >= from)
                {
                    peer.told_missed = peer.told_missed.saturating_add(1);
                    if peer.told_missed > granted && peer.pending_since.is_none() {
                        peer.pending_since = Some(now_ns);
                        peer.pending_nonce = Some(probe.nonce);
                        peer.report.pending_since_ns = Some(now_ns);
                        self.findings.push(Finding::Pending(unanswered));
                    }
                }
            }
            Liveness::Dead => {}
        }
    }

    /// An answer from `answered` to the probe `nonce` proves this member's own network works:
    /// every suspect whose told probe went unanswered is condemned now.
    fn condemn_pending(&mut self, answered: HostId, nonce: u64, now_ns: u64) {
        let mut suspects = std::mem::take(&mut self.aging);
        suspects.clear();
        suspects.extend(self.membership.suspects());
        let mut bound = None;
        for &(host, incarnation) in &suspects {
            let Some(peer) = self.peers.get_mut(&host) else {
                continue;
            };
            let Some(pending) = peer.pending_since else {
                continue;
            };
            if host == answered {
                continue;
            }
            let after = peer
                .last_answer_ns
                .map(|at| Duration::from_nanos(now_ns.saturating_sub(at)));
            // The bound to the pending condemnation, and the wait for an answer from another
            // member, measured as it happened.
            let within = bound
                .get_or_insert_with(|| self.detection_bound(now_ns))
                .map(|bound| {
                    bound.saturating_add(Duration::from_nanos(now_ns.saturating_sub(pending)))
                });
            if let Some(peer) = self.peers.get_mut(&host) {
                peer.report.condemnations = peer.report.condemnations.saturating_add(1);
                peer.report.condemned_after = after;
                peer.report.condemned_within = within;
            }
            // An update about a member the view holds is never refused.
            let _held = self.record(
                host,
                MemberState {
                    liveness: Liveness::Dead,
                    incarnation,
                },
            );
            self.findings.push(Finding::Condemned {
                target: host,
                pending_since_ns: pending,
                answered,
                nonce,
                at_ns: now_ns,
            });
        }
        self.aging = suspects;
    }

    /// The bound on the time from a peer's last answer to this member's condemnation of it pending,
    /// were it to crash then, `m` the members the view holds besides this one: its next probe is
    /// at most `2m − 1` periods away (SWIM §4.3), unanswered it suspects; the probe that tells it
    /// starts at most as far again, and when its own period resolves it unanswered the
    /// condemnation is pending. It then waits on an answer from another member, the evidence that
    /// this member's own network works, which nothing bounds in advance: the member measures that
    /// wait ([`PeerReport::pending_since_ns`]) and adds it. A period lasts at most the longest this
    /// member has run, or, where longer, what an unanswered probe's deadlines allow: its target's
    /// span, then its relay stage, until the slowest relay it could ask has its response due, plus
    /// the latest this member has woken past a wake it asked, or is late for now; the period in
    /// progress counts as run. `None` before a period.
    pub fn detection_bound(&self, now_ns: u64) -> Option<Duration> {
        let period = self.period_bound(now_ns)?;
        Some(Duration::from_nanos(
            period.saturating_mul(self.bound_periods()),
        ))
    }

    /// The least detection budget this member's measured deadlines support at `now_ns`:
    /// [`Detector::detection_bound`] with every period at what an unanswered probe's deadlines allow
    /// (its direct span and its relay stage, as the latest period began, plus how late this member
    /// has woken), not at the lengths its periods ran, which a budget's floor stretches. A budget
    /// below it cannot be kept by this member on this network; at it, a judged period's floor is the
    /// longest an unanswered period runs. `None` before a period.
    pub fn least_detection_bound(&self, now_ns: u64) -> Option<Duration> {
        if self.periods.count == 0 {
            return None;
        }
        let period = self
            .longest_unanswered
            .saturating_add(self.wakes.latest_ns(now_ns));
        Some(Duration::from_nanos(
            period.saturating_mul(self.bound_periods()),
        ))
    }

    /// The periods from a peer's last answer to its condemnation pending: `2(2m − 1) + 1`, `m` the
    /// most members the view has held besides this one. No round is larger, whatever the rounds in
    /// the window were (a member that condemned another, even falsely, runs smaller ones, and a
    /// member forgotten since was in rounds before); the told probe's own period resolves it.
    fn bound_periods(&self) -> u64 {
        let spacing = self.most_watched.saturating_mul(2).saturating_sub(1);
        spacing.saturating_mul(2).saturating_add(1)
    }

    /// The longest a period of this member lasts, nanoseconds: the longest it has run or, where
    /// longer, what an unanswered probe's deadlines allow, its direct span and then its relay stage
    /// ([`Detector::longest_unanswered`]), plus how late it has woken; the period in progress counts as
    /// run. `None` before a period.
    fn period_bound(&self, now_ns: u64) -> Option<u64> {
        if self.periods.count == 0 {
            return None;
        }
        // The period in progress and the wake it is late for are measured too: a stall the
        // member is in when it states the bound is in the bound.
        let running = self
            .probe
            .map_or(0, |probe| now_ns.saturating_sub(probe.sent_ns));
        let late = self.wakes.latest_ns(now_ns);
        let unanswered = self.longest_unanswered.saturating_add(late);
        Some(self.periods.longest.max(running).max(unanswered))
    }

    /// The dissemination window: SWIM's budget `T = λ ln n` periods, the largest budget this
    /// member's reports have had, each the longest a period of this member lasts (`docs/timing.md`
    /// §2.7). A rumor is sent on its adopter's next `T` messages, at least one a period, so past
    /// this after its last adoption it reaches nobody new. A dead member's record is kept for it,
    /// while gossip of the member from before its death can still arrive, and this member begins
    /// an anti-entropy exchange once each, so an update a rumor missed reaches the member it missed
    /// at the pace the rumor itself kept. `None` before a period.
    fn dissemination_window_ns(&self, now_ns: u64) -> Option<u64> {
        let period = self.period_bound(now_ns)?;
        Some(period.saturating_mul(u64::from(self.most_transmits)))
    }

    /// The deaths adopted since the last poll are seen now: their windows start at `now_ns`.
    fn stamp_deaths(&mut self, now_ns: u64) {
        for death in self.deaths.iter_mut().rev() {
            if death.at_ns.is_some() {
                break;
            }
            death.at_ns = Some(now_ns);
        }
    }

    /// The oldest death record past its window at `now_ns`, taken off the queue with the records
    /// before it that the member has since left; `None` while the oldest held is inside its window.
    fn expired(&mut self, now_ns: u64) -> Option<HostId> {
        let window = self.dissemination_window_ns(now_ns)?;
        while let Some(&death) = self.deaths.front() {
            if !death.held(&self.membership) {
                self.deaths.pop_front();
                continue;
            }
            let at = death.at_ns?;
            if now_ns.saturating_sub(at) < window {
                return None;
            }
            self.deaths.pop_front();
            return Some(death.member);
        }
        None
    }

    /// Forgets every dead member whose record is past its window, unless nobody alive or suspected
    /// is left: then the dead are the members this one probes.
    fn forget_expired(&mut self, now_ns: u64) {
        if self.deaths.is_empty() || self.isolated() {
            return;
        }
        while let Some(member) = self.expired(now_ns) {
            self.forget(member);
        }
    }

    /// Forgets `member`, held dead, and everything held of it. Its death has stood: the record
    /// outlived its window, which a refutation ends, so it counts as a failure of the fleet's
    /// (`Exposure`), once for each death, which its incarnation names ([`Detector::counted`]).
    /// Counted when adopted, a false condemnation, a live member's death refuted within the window,
    /// shortened the MTBF the margins are priced with, and a shorter MTBF buys smaller margins and
    /// more false condemnations (slates, 2026-10-10). A member that crashed and came back inside the
    /// window is not counted either; its record never stood.
    fn forget(&mut self, member: HostId) {
        let died = self.membership.state(member).map(|state| state.incarnation);
        if self.membership.forget(member) {
            if let Some(incarnation) = died
                && !self.counted.contains(&(member, incarnation))
            {
                self.exposure.on_failure();
                if self.counted.len() >= self.membership.capacity() {
                    self.counted.pop_front();
                }
                self.counted.push_back((member, incarnation));
            }
            if let Some(peer) = self.peers.remove(&member) {
                self.extremes
                    .change(member, peer.contribution, Contribution::default());
            }
            self.peer_coordinates.remove(&member);
            self.extensions.remove(&member);
            self.gossip.forget(member);
        }
    }

    /// Records an acknowledgement from `from` of the probe `nonce`, received at `at_ns` (the
    /// kernel's receive stamp where the caller has one, else when it was read), stating
    /// `relay_within_ns` ([`Ack::relay_within_ns`]), which bounds `from`'s responses when this
    /// member asks it to relay. It answers the period's probe if it is that probe's, and its round
    /// trip is measured whatever probe it answers, however late: a late answer is the tail the margin
    /// must cover.
    pub fn on_ack(&mut self, from: HostId, nonce: u64, at_ns: u64, relay_within_ns: u64) {
        match self.take_answer(from, nonce, at_ns, relay_within_ns) {
            Answered::Judge => self.respan(from),
            Answered::Restated => self.restate(from, relay_within_ns),
            Answered::Same => {}
        }
    }

    /// [`Detector::on_ack`]'s answer taken into the pair's evidence, and what it changed of how the
    /// pair is judged: its own verdict, which an answer changes only where it reconfigures it; the
    /// pool's or its timer's judgement, which every answer can move, where it has no verdict of its
    /// own; or only its stated relay bound, which states `G` and so moves at nearly every answer.
    fn take_answer(
        &mut self,
        from: HostId,
        nonce: u64,
        at_ns: u64,
        relay_within_ns: u64,
    ) -> Answered {
        // An answer to the period's probe answers it; any other is late, its period over.
        let current = match self.probe.as_mut() {
            Some(probe) if probe.target == from && probe.nonce == nonce => {
                probe.answered = true;
                true
            }
            _ => false,
        };
        self.clock_ns = self.clock_ns.max(at_ns);
        let measure = self.granularity().zip(self.periods.mean_ns());
        let interval = measure.map(|(_, period)| self.pair_interval(period));
        let mtbf = self.exposure.mtbf();
        let condemnation_cost = self.condemnation_cost;
        let pooled_rtt = self.pool_round_trip();
        let next = self.nonce;
        let Some(peer) = self.peers.get_mut(&from) else {
            return Answered::Same;
        };
        let verdict = peer.stream.verdict;
        let restated = peer.relay_within_ns != Some(relay_within_ns);
        // Any answer is evidence of life when it arrives, even one too late to be measured, and
        // states the peer's relay bound as it stands.
        peer.last_answer_ns = Some(peer.last_answer_ns.map_or(at_ns, |last| last.max(at_ns)));
        peer.relay_within_ns = Some(relay_within_ns);
        peer.answered_since_told(nonce, next);
        let Some(sent) = peer.take(nonce) else {
            // The latest reused record's answer is timed exactly: its pair's timer takes it, late.
            if let Some((round_trip, true)) = peer.answered_unrecorded(nonce, at_ns) {
                peer.rto.take(round_trip, true);
                self.least_rtt_ns = Some(
                    self.least_rtt_ns
                        .map_or(round_trip, |least| least.min(round_trip)),
                );
            }
            return Answered::of(verdict.is_none(), restated);
        };
        let rtt = at_ns.saturating_sub(sent.at_ns);
        // The pool takes the round trip of a pair whose path its earlier evidence showed to be the
        // pool's: judged before this sample, a stall on such a path is the host's, which the pool is
        // there to see (§2.6), and a far path's samples never are.
        let pools = peer.path_rtt_ns.is_some() && pooled_rtt.is_some() && !far(peer, pooled_rtt);
        peer.path_rtt_ns = Some(rtt);
        peer.rto.take(rtt, !current);
        self.least_rtt_ns = Some(self.least_rtt_ns.map_or(rtt, |least| least.min(rtt)));
        if let Some(((granularity, period), interval)) = measure.zip(interval) {
            peer.stream.take(sent.seq, rtt, granularity, interval);
            if peer.stream.due() {
                peer.stream
                    .configure(mtbf, granularity, interval, condemnation_cost);
                peer.report.configured = peer.stream.verdict.is_some();
            }
            // The pool is fed while it judges: by pairs with no verdict of their own, until it has
            // one; never by a far pair ([`far`]), whose round trips are another path's and would
            // stretch the pool's span, and with it every period it judges, over the paths it fits.
            if pools && (!peer.report.configured || self.pool.verdict.is_none()) {
                self.pool
                    .take(sent.nonce, rtt, granularity, Duration::from_nanos(period));
                if let Some(estimates) = self.pool.estimator.as_ref().map(|pool| pool.estimates()) {
                    self.pool_mean_ns = estimates.mean_delay.map(nanos);
                    self.pool_counts = (estimates.lost, estimates.received);
                }
            }
        } else {
            self.unmeasured = self.unmeasured.saturating_add(1);
        }
        let rejudge = Answered::of(
            verdict.is_none() || peer.stream.verdict != verdict,
            restated,
        );
        // The error estimate remembers one round: one probe of each member the round holds.
        if let Some(coordinate) = self.peer_coordinates.get(&from) {
            let round = self.order.len();
            self.coordinates
                .update(coordinate, Duration::from_nanos(rtt), round);
        }
        rejudge
    }

    /// The pool's round trip, nanoseconds: the mean of the round trips its estimator holds, and
    /// before it holds one, the least round trip this member measured exactly ([`far`]).
    fn pool_round_trip(&self) -> Option<u64> {
        self.pool_mean_ns
            .filter(|mean| *mean > 0)
            .or(self.least_rtt_ns)
    }

    /// The pool's verdict for a probe of a pair that has none of its own, renewed when due.
    fn pooled(&mut self) -> Option<Verdict> {
        if self.pool.due()
            && let Some((granularity, period)) = self.granularity().zip(self.periods.mean_ns())
        {
            let interval = self.pair_interval(period);
            let before = self.pool.verdict;
            self.pool.configure(
                self.exposure.mtbf(),
                granularity,
                interval,
                self.condemnation_cost,
            );
            if self.pool.verdict != before {
                self.pool_epoch = self.pool_epoch.wrapping_add(1);
            }
        }
        self.pool.verdict
    }

    /// The pair's probe interval: the round, one period for each member watched.
    fn pair_interval(&self, period_ns: u64) -> Duration {
        let watched = u64::try_from(self.order.len()).unwrap_or(1).max(1);
        Duration::from_nanos(period_ns.saturating_mul(watched))
    }

    /// `G`, the mean lateness of this member's wakes, once one is measured, and never below the
    /// clock's resolution ([`Detector::new`]).
    pub fn granularity(&self) -> Option<Duration> {
        self.wakes.granularity()
    }

    /// Round trips this member measured before it had a wake and a period measured, which no
    /// estimator took (each pair's timer did).
    pub fn unmeasured(&self) -> u64 {
        self.unmeasured
    }

    /// Records an indirect acknowledgement that `target` answered this member's probe `nonce`
    /// through a relay, at `at_ns`: the period's probe is answered, if it is that one. However late,
    /// it is evidence of the target's life when it arrives, and an answer to the told probe or a later
    /// one takes back a condemnation pending on it. A relayed round trip is two paths' and is no
    /// pair's sample.
    pub fn on_indirect_ack(&mut self, target: HostId, nonce: u64, at_ns: u64) {
        self.clock_ns = self.clock_ns.max(at_ns);
        if let Some(probe) = self.probe.as_mut()
            && probe.target == target
            && probe.nonce == nonce
        {
            probe.answered = true;
        }
        // Only an answer to a probe of this member's own is its evidence.
        let next = self.nonce;
        if nonce < next
            && let Some(peer) = self.peers.get_mut(&target)
        {
            peer.last_answer_ns = Some(peer.last_answer_ns.map_or(at_ns, |last| last.max(at_ns)));
            peer.answered_since_told(nonce, next);
        }
    }

    /// Records a relay's nack at `at_ns`: `relay`, asked to probe `target` for this member's probe
    /// `nonce`, had no answer from it by its own deadline for it, as the relayed [`Ping::due_ns`]
    /// stated (Lifeguard's nack, Dadgar et al. §IV-A): the relay judged its own leg to the target by
    /// its own measure, which this member cannot take. It is no evidence of life; when every relay
    /// the period's probe asked has nacked, the relay stage ends, unanswered.
    pub fn on_indirect_nack(&mut self, relay: HostId, target: HostId, nonce: u64, at_ns: u64) {
        self.clock_ns = self.clock_ns.max(at_ns);
        if self
            .probe
            .is_some_and(|probe| probe.target == target && probe.nonce == nonce)
        {
            self.awaiting.retain(|asked| *asked != relay);
        }
    }

    /// Responds to a ping from `from` with the acknowledgement to send back, which states how long
    /// after a ping-request from `from` reaches this member it responds ([`Ack::relay_within_ns`]).
    pub fn on_ping(&mut self, from: HostId) -> Ack {
        self.refresh_extremes();
        Ack {
            to: from,
            relay_within_ns: self.relay_within_ns(from),
        }
    }

    /// As a relay, the ping to send `target` at `now_ns` for `asker`'s ping-request; its nonce is
    /// the relay's own, from a range its own probes never use, and the caller maps the answer back to
    /// the asker ([`SwimMessage::IndirectAck`](crate::codec::SwimMessage::IndirectAck)). Its
    /// deadline, [`Ping::due_ns`], is this member's own for its probes of the target, as it judges
    /// them now, within the longest it held for any member but the asker as the latest
    /// period began, which the bound it states to the asker covers ([`Ack::relay_within_ns`]); a
    /// target it does not hold is given that longest. With no answer by then the caller tells the
    /// asker so ([`SwimMessage::Nack`](crate::codec::SwimMessage::Nack)), and an answer after it
    /// still goes back as an indirect acknowledgement: each leg of the relayed exchange is judged by
    /// the member that measures it (Lifeguard's nack, Dadgar et al. §IV-A).
    pub fn on_ping_req(&mut self, asker: HostId, target: HostId, now_ns: u64) -> Ping {
        self.clock_ns = self.clock_ns.max(now_ns);
        let nonce = self.relayed;
        self.relayed = self.relayed.saturating_sub(1);
        self.refresh_extremes();
        let longest = self.extremes.without(asker);
        let (pooled, loss, granularity_ns) =
            (self.pool.verdict, self.probe_loss(), self.granularity_ns());
        let span = self.peers.get(&target).map_or(longest, |peer| {
            judging(peer, pooled, loss, granularity_ns)
                .0
                .span_ns()
                .min(longest)
        });
        Ping {
            to: target,
            nonce,
            due_ns: now_ns.saturating_add(span),
        }
    }

    /// What the latest [`poll`](Detector::poll) found by this member's own probes, in the order
    /// found, each with its evidence; replaced at every poll.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// What this member has done and promised about `peer`.
    pub fn report(&self, peer: HostId) -> Option<PeerReport> {
        self.peers.get(&peer).map(|held| PeerReport {
            judge: match judging(
                held,
                self.pool.verdict,
                self.probe_loss(),
                self.granularity_ns(),
            ) {
                (_, true) => Judge::Timer,
                (_, false) if held.stream.verdict.is_some() => Judge::Own,
                (_, false) => Judge::Pool,
            },
            timer: {
                let (floor, minimum) = timer_floors(self.pool.verdict, self.granularity_ns());
                held.rto.report(floor, minimum)
            },
            last_answer_ns: held.last_answer_ns,
            relay_within: held.relay_within_ns.map(Duration::from_nanos),
            pool_misfit: far(held, self.pool_round_trip()),
            refused: held.stream.refused,
            ..held.report
        })
    }

    /// The relay stage as this member bounds it now, every relay it could ask judged afresh.
    pub fn relays(&self) -> RelayReport {
        let (pooled, granularity_ns) = (self.pool.verdict, self.granularity_ns());
        RelayReport {
            due: self
                .peers
                .iter()
                .filter_map(|(host, peer)| {
                    self.contribution(*host, peer, pooled, granularity_ns)
                        .relay_due_ns
                })
                .max()
                .map(Duration::from_nanos),
            stated: self
                .peers
                .values()
                .filter(|peer| peer.relay_within_ns.is_some())
                .count(),
        }
    }

    /// The member's pooled verdict and its evidence.
    pub fn pool(&self) -> PoolReport {
        PoolReport {
            verdict: self.pool.verdict,
            samples: self.pool.samples,
            refused: self.pool.refused,
        }
    }

    /// The round trips the pair's own estimator has taken of `peer`: the evidence it gathers
    /// toward its own configuration, whose longest window, `hyper_timing::WINDOW_LIMIT`, bounds
    /// what a configuration can need.
    pub fn round_trips_taken(&self, peer: HostId) -> Option<u64> {
        self.peers.get(&peer).map(|held| held.stream.samples)
    }

    /// The verdict that times this member's probes of `peer` now: the pair's own;
    /// before it has one, the pool's where the pool's span covers the pair, else the pair's timer's
    /// (`mistake` 1). `None` for a member the view does not hold.
    pub fn verdict(&self, peer: HostId) -> Option<Verdict> {
        self.peers.get(&peer).map(|held| {
            judging(
                held,
                self.pool.verdict,
                self.probe_loss(),
                self.granularity_ns(),
            )
            .0
        })
    }

    /// The failure history the margins are priced with: the node time this member watched beside
    /// the owner's seed, and the deaths that stood ([`Exposure::mtbf`]).
    pub fn exposure(&self) -> Exposure {
        self.exposure
    }

    /// The mean of this member's periods, once one has ended.
    pub fn mean_period(&self) -> Option<Duration> {
        self.periods.mean_ns().map(Duration::from_nanos)
    }

    /// A suspected `subject` asks for more time with a progress `witness` it cannot fake while
    /// stuck, saying whether it is `overloaded` (mantle note 32 S13; focal's witnessed extensions).
    /// The base window is the one probe that tells a suspect, so a grant is one more probe.
    pub fn request_extension(
        &mut self,
        subject: HostId,
        witness: u64,
        overloaded: bool,
    ) -> ExtensionDecision {
        let suspected = self
            .membership
            .state(subject)
            .is_some_and(|state| state.liveness == Liveness::Suspect);
        if !suspected {
            return ExtensionDecision::Denied(ExtensionDenial::NotSuspected);
        }
        self.extensions
            .entry(subject)
            .or_default()
            .request(self.period, 1, witness, overloaded)
    }

    /// This node's own network coordinate, to gossip so peers can predict the round-trip time to it.
    pub fn coordinate(&self) -> &NetworkCoordinate {
        self.coordinates.coordinate()
    }

    /// Learns `peer`'s network coordinate (from a probe reply or gossip). Only a member this node
    /// probes is learned, and a member's coordinate is forgotten when it is declared dead, so the
    /// coordinates held are bounded by the membership. A coordinate that cannot be used
    /// ([`NetworkCoordinate::is_usable`]) is not learned: the one held, if any, stays.
    pub fn learn_coordinate(&mut self, peer: HostId, coordinate: Coordinate<'_>) {
        if peer == self.local || !self.is_probed(peer) {
            return;
        }
        let coordinate = coordinate.to_coordinate();
        if !coordinate.is_usable() {
            return;
        }
        match self.peer_coordinates.get_mut(&peer) {
            Some(held) => *held = coordinate,
            None => {
                self.peer_coordinates.insert(peer, coordinate);
            }
        }
    }

    /// The predicted round-trip time from this node to `peer`, in seconds, when `peer`'s coordinate
    /// is known.
    pub fn predicted_rtt(&self, peer: HostId) -> Option<f64> {
        self.peer_coordinates
            .get(&peer)
            .map(|coordinate| self.coordinates.predict(coordinate))
    }

    /// The predicted round-trip time between two peers whose coordinates this node has learned.
    fn predicted_between(&self, from: HostId, to: HostId) -> Option<f64> {
        let from_coordinate = self.peer_coordinates.get(&from)?;
        let to_coordinate = self.peer_coordinates.get(&to)?;
        Some(CoordinateEngine::estimate_rtt(
            from_coordinate,
            to_coordinate,
        ))
    }

    /// Applies a membership update, enqueues the change for gossip, and keeps the peer's suspicion
    /// state in step with the view. An update about one member more than the view holds takes the
    /// place of a dead member whose record is past its window, or is refused.
    fn record(&mut self, subject: HostId, update: MemberState) -> Result<Option<Change>, Full> {
        let change = match self.membership.apply(subject, update) {
            Err(Full) => match self.expired(self.clock_ns) {
                Some(member) => {
                    self.forget(member);
                    self.membership.apply(subject, update)
                }
                None => Err(Full),
            },
            applied => applied,
        };
        let change = match change {
            Ok(change) => change,
            Err(Full) => {
                self.refused = self.refused.saturating_add(1);
                return Err(Full);
            }
        };
        let watched = u64::try_from(self.membership.len().saturating_sub(1)).unwrap_or(u64::MAX);
        self.most_watched = self.most_watched.max(watched);
        match change {
            Some(Change::Adopted { member, state }) => {
                self.gossip.record(member, state);
                self.adopted(member, state);
            }
            Some(Change::Refuted { incarnation }) => {
                let state = MemberState {
                    liveness: Liveness::Alive,
                    incarnation,
                };
                self.gossip.record(self.local, state);
            }
            None => {}
        }
        Ok(change)
    }

    fn adopted(&mut self, member: HostId, state: MemberState) {
        let peer = self.peers.entry(member).or_insert_with(Peer::new);
        peer.liveness = Some(state.liveness);
        match state.liveness {
            Liveness::Alive => peer.clear_suspicion(),
            // A suspicion already held, adopted again at a newer incarnation, keeps the probes
            // that told the peer: they carried a suspicion and went unanswered all the same.
            Liveness::Suspect if peer.suspected_from.is_none() => {
                peer.suspected_from = Some(peer.sent);
                peer.clear_pending();
            }
            Liveness::Suspect => {}
            Liveness::Dead => {
                // The report keeps when the condemnation was pending, for the bound it is held to.
                peer.suspected_from = None;
                peer.told_missed = 0;
                peer.pending_since = None;
                self.extensions.remove(&member);
                self.peer_coordinates.remove(&member);
                self.record_death(Death {
                    member,
                    incarnation: state.incarnation,
                    at_ns: None,
                });
            }
        }
        self.respan(member);
    }

    /// Queues a death's record. Records the member has since left are purged once the queue holds
    /// twice the view's bound, so it holds at most that, plus one.
    fn record_death(&mut self, death: Death) {
        if self.deaths.len() >= self.membership.capacity().saturating_mul(2) {
            let membership = &self.membership;
            self.deaths.retain(|held| held.held(membership));
        }
        self.deaths.push_back(death);
    }

    /// The batch of membership updates to piggyback on an outgoing message: up to `max`, the
    /// least-disseminated first, each sent its budget ([`gossip_transmits`]) and then dropped.
    /// The batch replaces what `batch` held; `batch` keeps its capacity.
    pub fn gossip_into(&mut self, max: usize, batch: &mut Vec<(HostId, MemberState)>) {
        self.gossip.drain(max, self.transmits, batch);
    }

    /// The gossip batch to piggyback on a probe of `target`: the ordinary batch; this member's own
    /// state, alive at its incarnation, so `target` learns it from every probe; and, while this
    /// member suspects `target` or holds it dead, that belief, even after its transmit budget is
    /// spent (Lifeguard's buddy system, §IV-C), so `target` hears it from the probe it answers and
    /// refutes at once. A refutation is a rumor, and a rumor can end known to some members and not
    /// all (Demers et al. 1987, §1.5): a member it missed holds the refuted member dead, or has
    /// forgotten it, and probes it no more, but the refuted member still probes it, and its probes
    /// carry the refutation. Within `max`: the two take their room first, the belief before the
    /// state, and the ordinary batch the rest.
    pub fn ping_gossip_into(
        &mut self,
        target: HostId,
        max: usize,
        batch: &mut Vec<(HostId, MemberState)>,
    ) {
        let belief = self.belief(target);
        let own = MemberState {
            liveness: Liveness::Alive,
            incarnation: self.membership.local_incarnation(),
        };
        self.gossip_with(max, [belief, Some((self.local, own))], batch);
    }

    /// The gossip batch to piggyback on the answer to a probe from `prober`: the ordinary batch
    /// and, while this member suspects `prober` or holds it dead, that belief. Nobody probes the
    /// dead, so the answer is where a member held dead hears it, at the incarnation it died at, and
    /// refutes; its next probe revives it here. Within `max`: the belief takes its room first.
    pub fn ack_gossip_into(
        &mut self,
        prober: HostId,
        max: usize,
        batch: &mut Vec<(HostId, MemberState)>,
    ) {
        let belief = self.belief(prober);
        self.gossip_with(max, [belief, None], batch);
    }

    /// What this member holds against `peer`: its suspicion or death, if it holds either.
    fn belief(&self, peer: HostId) -> Option<(HostId, MemberState)> {
        self.membership
            .state(peer)
            .filter(|state| state.liveness != Liveness::Alive)
            .map(|state| (peer, state))
    }

    /// The ordinary batch in the room `entries` leave of `max`, then `entries`, in order, within
    /// `max`. A rumor is drained only into room it is sent in, so none is counted sent that was
    /// not; an entry that repeats a rumor of the same state is applied twice, to no effect.
    fn gossip_with(
        &mut self,
        max: usize,
        entries: [Option<(HostId, MemberState)>; 2],
        batch: &mut Vec<(HostId, MemberState)>,
    ) {
        let forced = entries.iter().flatten().count();
        self.gossip_into(max.saturating_sub(forced), batch);
        for entry in entries.into_iter().flatten() {
            if batch.len() < max {
                batch.push(entry);
            }
        }
    }

    /// Applies a received gossip batch, folding each update into the view (and re-enqueueing
    /// anything it adopts so the change spreads onward). An update the view's bound refuses is
    /// counted ([`Detector::refused`]) and the rest still apply.
    pub fn apply_gossip(&mut self, updates: impl IntoIterator<Item = (HostId, MemberState)>) {
        for (subject, state) in updates {
            let _refused_and_counted = self.apply(subject, state);
        }
    }

    /// Updates the view's bound refused, as [`Full`]: an update about one member more than it
    /// holds while no dead member's record was past its window.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// The next message of anti-entropy to send (Demers et al. 1987, §1.3 and §1.5), and to whom:
    /// the push of its view this member owes first, then the opening of an exchange it began.
    /// Fills `batch` with up to `max` members of its view, each with its state, its own included,
    /// in id order after the last it sent, replacing what `batch` held: none for an opening, which
    /// carries the view's digest alone. `None` once nothing is left to send. The owner sends each as
    /// a [`SwimMessage::Sync`](crate::codec::SwimMessage::Sync) and asks again until `None`, after
    /// each [`poll`](Detector::poll) and each message it takes. An exchange begins once a
    /// dissemination window after the last, with the next partner of a shuffled cycle of the
    /// members this one holds alive; one whose opening the owner has not taken is let go first.
    pub fn sync_into(
        &mut self,
        max: usize,
        batch: &mut Vec<(HostId, MemberState)>,
    ) -> Option<ViewChunk> {
        batch.clear();
        let digest = self.membership.digest();
        if let Some(push) = self.exchanges.answer.take()
            && max > 0
        {
            batch.extend(self.membership.after(push.after).take(max));
            if let Some(&(last, _)) = batch.last() {
                let more = self.membership.after(Some(last)).next().is_some();
                self.exchanges.answer = more.then_some(Push {
                    after: Some(last),
                    pull: false,
                    ..push
                });
                return Some(ViewChunk {
                    to: push.to,
                    pull: push.pull,
                    digest,
                });
            }
        }
        let to = self.exchanges.opening.take()?;
        Some(ViewChunk {
            to,
            pull: true,
            digest,
        })
    }

    /// Takes a message of anti-entropy from `from` (a
    /// [`SwimMessage::Sync`](crate::codec::SwimMessage::Sync)) carrying the digest of its view:
    /// each member's state it carries applied as gossip is. An opening, carrying none, whose digest
    /// differs from this member's earns a push of this view asking `from`'s in return; one whose
    /// digest is the same earns nothing, the views agreeing. A push that asks a pull earns a push of
    /// this view. A member owes one push at a time: a pull from another while it owes one is
    /// refused and counted ([`pulls_refused`](Detector::pulls_refused)).
    pub fn on_sync(
        &mut self,
        from: HostId,
        digest: u64,
        pull: bool,
        entries: impl IntoIterator<Item = (HostId, MemberState)>,
    ) {
        let mut carried = false;
        for (subject, state) in entries {
            carried = true;
            // An update about one member more than the view holds is refused, and counted.
            let _refused_and_counted = self.apply(subject, state);
        }
        if !pull {
            return;
        }
        let ask = if carried {
            false
        } else if digest != self.membership.digest() {
            true
        } else {
            return;
        };
        match self.exchanges.answer {
            None => {
                self.exchanges.answer = Some(Push {
                    to: from,
                    after: None,
                    pull: ask,
                });
            }
            Some(answer) if answer.to == from => {}
            Some(_) => self.exchanges.refused = self.exchanges.refused.saturating_add(1),
        }
    }

    /// Pulls refused because an answer to another member was owed ([`on_sync`](Detector::on_sync)).
    pub fn pulls_refused(&self) -> u64 {
        self.exchanges.refused
    }

    /// Begins an exchange once a dissemination window has passed since the last began: an opening
    /// to the next partner, carrying this view's digest and asking the partner's view if its own
    /// differs. One whose opening the owner has not taken is let go first.
    fn begin_exchange(&mut self, now_ns: u64) {
        if self.exchanges.opening.is_some() {
            return;
        }
        let Some(window) = self.dissemination_window_ns(now_ns) else {
            return;
        };
        if self
            .exchanges
            .began_ns
            .is_some_and(|began| now_ns.saturating_sub(began) < window)
        {
            return;
        }
        self.exchanges.began_ns = Some(now_ns);
        self.exchanges.opening = self.next_partner();
    }

    /// The next partner in a shuffled cycle of the members this one holds alive, besides itself,
    /// rebuilt when it runs out: over a cycle the member exchanges with every member it held alive
    /// when the cycle began. `None` when it holds nobody else alive.
    fn next_partner(&mut self) -> Option<HostId> {
        loop {
            while let Some(&candidate) = self.exchanges.partners.get(self.exchanges.cursor) {
                self.exchanges.cursor = self.exchanges.cursor.saturating_add(1);
                if self
                    .membership
                    .state(candidate)
                    .is_some_and(|state| state.liveness == Liveness::Alive)
                {
                    return Some(candidate);
                }
            }
            let local = self.local;
            self.exchanges.partners.clear();
            self.exchanges
                .partners
                .extend(self.membership.alive().filter(|host| *host != local));
            if self.exchanges.partners.is_empty() {
                return None;
            }
            self.shuffler.shuffle(&mut self.exchanges.partners);
            self.exchanges.cursor = 0;
        }
    }

    /// The membership view this detector maintains.
    pub fn membership(&self) -> &Membership {
        &self.membership
    }

    /// Learns a peer (alive at incarnation zero) — a join. A later round will probe it. Refused
    /// when the view holds its bound ([`Full`]).
    pub fn join(&mut self, peer: HostId) -> Result<(), Full> {
        if peer != self.local {
            self.record(
                peer,
                MemberState {
                    liveness: Liveness::Alive,
                    incarnation: 0,
                },
            )?;
        }
        Ok(())
    }

    /// Learns a peer as [`join`](Self::join) does, with a round trip to it that the owner measured
    /// outside the detector, the handshake that keyed the peer's session: the pair's timer takes it
    /// as its first round trip, as TCP's takes its handshake's (RFC 6298 (2.2)), so the pair's
    /// probes are timed from the path's own round trip and not from the initial second, some 10⁴ of
    /// a LAN's. No estimator takes it: a handshake is not a probe. A zero round trip measures nothing
    /// and is not taken.
    pub fn join_measured(&mut self, peer: HostId, round_trip: Duration) -> Result<(), Full> {
        self.join(peer)?;
        let rtt = u64::try_from(round_trip.as_nanos()).unwrap_or(u64::MAX);
        if let Some(held) = self.peers.get_mut(&peer)
            && rtt > 0
        {
            held.rto.take(rtt, false);
            held.path_rtt_ns = held.path_rtt_ns.or(Some(rtt));
            self.least_rtt_ns = Some(self.least_rtt_ns.map_or(rtt, |least| least.min(rtt)));
            self.respan(peer);
        }
        Ok(())
    }

    /// Applies a gossiped membership update, returning the change and enqueuing it for onward
    /// gossip; refused, and counted, when it is about one member more than the view holds.
    pub fn apply(&mut self, subject: HostId, update: MemberState) -> Result<Option<Change>, Full> {
        self.record(subject, update)
    }

    /// Whether this node holds no other member alive or suspected.
    fn isolated(&self) -> bool {
        let local = self.local;
        self.membership.alive().all(|host| host == local)
            && self.membership.suspects().next().is_none()
    }

    /// Whether `member` is one this node probes: alive or suspected — a member until it is dead.
    fn is_probed(&self, member: HostId) -> bool {
        matches!(
            self.membership.state(member).map(|state| state.liveness),
            Some(Liveness::Alive | Liveness::Suspect)
        )
    }

    /// The next peer to probe — alive or suspected — in randomized order (SWIM §4.3). A suspect is
    /// still a member and is probed until it is dead, so the probe that tells it is sent and its
    /// answer counts. Each round is a fresh permutation; members dead mid-round are skipped. A
    /// member with nobody alive or suspected left probes those it holds dead. The dissemination
    /// budget follows the membership at each round.
    fn next_target(&mut self) -> Option<HostId> {
        loop {
            while let Some(&candidate) = self.order.get(self.cursor) {
                self.cursor = self.cursor.saturating_add(1);
                // Isolated, it probes the dead it still holds; one forgotten mid-round is no target.
                if self.is_probed(candidate)
                    || (self.membership.state(candidate).is_some() && self.isolated())
                {
                    return Some(candidate);
                }
            }
            let local = self.local;
            let suspected = self.membership.suspects().map(|(host, _)| host);
            self.order.clear();
            self.order.extend(
                self.membership
                    .alive()
                    .chain(suspected)
                    .filter(|host| *host != local),
            );
            if self.order.is_empty() {
                // Nobody left alive in this member's view: it is likelier the one cut off than
                // every other member dead (Lifeguard §IV), so it probes the members it holds dead;
                // each ping tells its target so, and a live one refutes in its answer.
                self.order.extend(self.membership.dead());
            }
            if self.order.is_empty() {
                return None;
            }
            self.transmits = gossip_transmits(self.order.len().saturating_add(1));
            self.most_transmits = self.most_transmits.max(self.transmits);
            self.shuffler.shuffle(&mut self.order);
            self.cursor = 0;
        }
    }
}

/// Feeds a round trip as heartbeat `seq` on the estimator's schedule: arrival
/// `(seq − anchor)·η + rtt`, so the offset it measures is the round trip itself. The schedule is
/// the estimator's own interval, which tracks the pair's real spacing on average; a probe's
/// spacing does not enter NFD-E's offset.
fn feed(
    estimator: &mut LinkEstimator,
    seq: u64,
    anchor: u64,
    rtt: u64,
) -> Result<(), hyper_timing::EstimateError> {
    let interval = nanos(estimator.interval());
    let arrival = seq
        .checked_sub(anchor)
        .and_then(|steps| steps.checked_mul(interval))
        .and_then(|scheduled| scheduled.checked_add(rtt))
        .ok_or(hyper_timing::EstimateError::OutOfRange)?;
    estimator.on_heartbeat(seq, arrival).map(|_| ())
}

/// The verdict for a probe stream: the margin minimizing unavailability at the pair's interval
/// (`docs/timing.md` §2.7, "The margin"). A false suspicion costs the time until it is refuted, the
/// pair's next probe, which carries it, answered: `η + μ`. A false condemnation costs the owner's
/// response to a death, `condemnation_cost`, and comes at most as often as a live peer's probe and
/// the one before both miss, which Fréchet's bound puts at the lesser of their two bounds, so each
/// probe is charged `(η + μ + condemnation_cost)·β`. A crash costs its detection.
///
/// A pair's probes never overlap: a judged period runs to its probe's deadline, so the next probe
/// of the pair is sent after it. The bound on a live peer's probe missing its deadline is therefore
/// one Cantelli factor at any margin, `β = p + (1 − p)·V/(V + α²)` (Theorem 7's with one probe
/// fresh), and the margin is searched over every margin: the per-arrival configurator
/// (`hyper_timing::arrival_detector_at`) with the round trip's lateness measured from its mean and
/// the loss in place of the unseen share. Theorem 7's product capped the margin at `η − G` to keep
/// one factor, and early on, `η` drawn from measurement periods, the cap held a verdict at a bound
/// of 0.713 (slates, 2026-10-10: `η` about 500 µs, `G` 375 µs, the margin 125 µs).
fn verdict(
    estimator: &LinkEstimator,
    mtbf: Option<Duration>,
    granularity: Duration,
    interval: Duration,
    condemnation_cost: Duration,
) -> Result<Verdict, Refusal> {
    let link: LinkBehaviour = estimator.behaviour()?;
    let mtbf = mtbf.ok_or(Refusal::Unconfigurable)?;
    let costs = Costs {
        election: interval
            .saturating_add(link.mean_delay)
            .saturating_add(condemnation_cost),
        mtbf,
    };
    let probes = Arrivals {
        unseen: link.loss,
        lateness: Duration::ZERO,
        deviation: link.delay_deviation,
        mean_delay: link.mean_delay,
    };
    let configured = arrival_detector_at(&probes, &costs, granularity, interval)
        .ok_or(Refusal::Unconfigurable)?;
    Ok(Verdict {
        round_trip: link.mean_delay,
        margin: configured.margin,
        interval,
        loss: link.loss,
        mistake: lateness_bound(&probes, configured.margin),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: HostId = HostId(1);
    const A: HostId = HostId(2);
    const B: HostId = HostId(3);
    const C: HostId = HostId(4);
    const MS: u64 = 1_000_000;

    impl Detector {
        /// Checks what this member keeps of its extremes against what judging every peer now
        /// gives: each peer's contribution as kept, and the extremes taken from scratch. Every
        /// driver below checks it after each poll and each answer, so a change to a pair that
        /// left it unjudged fails the test that makes it.
        fn check_extremes(&mut self) {
            self.refresh_extremes();
            let (pooled, granularity_ns) = (
                self.pool.verdict,
                self.granularity().map_or(self.resolution_ns, nanos),
            );
            assert_eq!(self.granularity_ns(), granularity_ns, "G as kept");
            let estimates = self.pool.estimator.as_ref().map(|pool| pool.estimates());
            assert_eq!(
                (self.pool_mean_ns, self.pool_counts),
                estimates.map_or((None, (0, 0)), |estimates| (
                    estimates.mean_delay.map(nanos),
                    (estimates.lost, estimates.received)
                )),
                "the pool's mean and counts as kept"
            );
            let mut scratch = Extremes::default();
            for (host, peer) in &self.peers {
                assert_eq!(
                    peer.liveness,
                    self.membership.state(*host).map(|state| state.liveness),
                    "{host:?}'s liveness as kept"
                );
                let fresh = self.contribution(*host, peer, pooled, granularity_ns);
                assert_eq!(peer.contribution, fresh, "{host:?}'s contribution as kept");
                scratch.change(*host, Contribution::default(), fresh);
            }
            assert_eq!(
                (
                    self.extremes.first,
                    self.extremes.second,
                    self.extremes.relay
                ),
                (scratch.first, scratch.second, scratch.relay),
                "the extremes as kept"
            );
        }
    }

    /// A xorshift stream (Marsaglia 2003): deterministic test noise.
    fn noise(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// The simulated clock's resolution: its readings are whole nanoseconds.
    const RESOLUTION: Duration = Duration::from_nanos(1);

    /// Room for every member a test names.
    fn room() -> NonZeroUsize {
        NonZeroUsize::new(16).unwrap()
    }

    /// A detector for `LOCAL` that knows `peers`.
    fn detector(peers: &[HostId]) -> Detector {
        let mut detector =
            Detector::new(LOCAL, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        for &peer in peers {
            detector.join(peer).unwrap();
        }
        detector
    }

    /// The driver of one detector over simulated time: answers each probe after the round trip
    /// `answer` gives (none for silence), wakes `late` after each wake asked, and, when nothing
    /// is due, hears a ping from `heard`.
    struct World {
        now: u64,
        late: u64,
        heard: HostId,
        pings: Vec<Ping>,
        requests: Vec<PingReq>,
        asked: Vec<PingReq>,
        /// What each poll found, in order.
        found: Vec<Finding>,
    }

    impl World {
        fn new() -> Self {
            Self {
                now: MS,
                late: MS / 10,
                heard: C,
                pings: Vec::new(),
                requests: Vec::new(),
                asked: Vec::new(),
                found: Vec::new(),
            }
        }

        /// Runs `periods` periods.
        fn run(
            &mut self,
            detector: &mut Detector,
            periods: usize,
            mut answer: impl FnMut(HostId) -> Option<u64>,
        ) {
            let mut pending: Option<(HostId, u64, u64)> = None;
            let mut started = 0;
            while started <= periods {
                let ping = detector.poll(self.now, &mut self.requests);
                detector.check_extremes();
                self.found.extend_from_slice(detector.findings());
                if let Some(ping) = ping {
                    self.pings.push(ping);
                    started += 1;
                    pending = answer(ping.to).map(|rtt| (ping.to, ping.nonce, self.now + rtt));
                }
                self.asked.extend(self.requests.iter().copied());
                let ack = pending.map(|(_, _, at)| at);
                match (detector.wake().map(|w| w + self.late), ack) {
                    (Some(wake), Some(at)) => self.now = wake.min(at).max(self.now),
                    (Some(wake), None) => self.now = wake.max(self.now),
                    (None, Some(at)) => self.now = at.max(self.now),
                    (None, None) => {
                        self.now += MS;
                        detector.on_ping(self.heard);
                    }
                }
                if let Some((from, nonce, at)) = pending
                    && at <= self.now
                {
                    detector.on_ack(from, nonce, at, detector.relay_within_ns(from));
                    detector.check_extremes();
                    pending = None;
                }
            }
        }
    }

    /// A round trip of 1 ms and up to 0.5 ms of jitter.
    fn jitter(state: &mut u64) -> u64 {
        MS + noise(state) % (MS / 2)
    }

    /// Runs until every pair is configured by its own estimator, everyone answering.
    fn configured(detector: &mut Detector, world: &mut World, peers: &[HostId]) {
        let mut state = 0x2545_F491_4F6C_DD1D;
        for _ in 0..100 {
            world.run(detector, 100, |_| Some(jitter(&mut state)));
            if peers
                .iter()
                .all(|peer| detector.report(*peer).is_some_and(|r| r.configured))
            {
                return;
            }
        }
        panic!("the pairs never configured");
    }

    /// The direct deadline of the period's probe.
    fn ping_sent(detector: &Detector) -> u64 {
        detector.probe.unwrap().due_ns()
    }

    fn liveness(detector: &Detector, peer: HostId) -> Liveness {
        detector.membership().state(peer).unwrap().liveness
    }

    /// A probe whose ping or answer was lost is judged at its pair's timer: its period ends at the
    /// timeout and then its relays', and the member probes on. An end that waited on another member
    /// left every member waiting for ever once all their probes were lost at once (a throttled
    /// container dropping a burst). Before the pool has measured the host's stalls, the timeout is at
    /// least RFC 6298's second (2.4).
    #[test]
    fn a_lost_probe_is_judged_at_its_pair_s_timer() {
        let mut detector = detector(&[A, B]);
        let mut requests = Vec::new();
        let first = detector.poll(0, &mut requests).unwrap();
        detector.on_ack(
            first.to,
            first.nonce,
            MS,
            detector.relay_within_ns(first.to),
        );
        let lost = detector.poll(MS, &mut requests).unwrap();
        let timeout = MS + MINIMUM_RTO_NS;
        assert_eq!(lost.due_ns, timeout, "judged at the timer's second");
        assert_eq!(detector.poll(timeout - 1, &mut requests), None);
        assert_eq!(detector.poll(timeout, &mut requests), None);
        assert_eq!(requests.len(), 1, "the other member asked to relay");
        let relays_due = detector.wake().unwrap();
        assert!(
            detector.poll(relays_due, &mut requests).is_some(),
            "the period ended and the member probes on"
        );
        assert_eq!(
            liveness(&detector, lost.to),
            Liveness::Suspect,
            "judged, and suspected"
        );
    }

    /// A first probe, before any round trip of its pair, is judged at RFC 6298's initial second
    /// (2.1), and an unanswered one doubles its pair's next timeout (5.5), while another pair's stays
    /// its own: a pair's timer backs off as a connection's does. An end that waited on another
    /// member instead left members whose first probes were all lost waiting on one another for ever
    /// (slates' daemons, the datagrams queued at a re-key dropped).
    #[test]
    fn a_first_probe_is_judged_at_the_initial_second_and_its_pair_backs_off() {
        let mut detector = detector(&[A, B]);
        let mut requests = Vec::new();
        let first = detector.poll(0, &mut requests).unwrap();
        assert_eq!(first.due_ns, INITIAL_RTO_NS, "the initial second");
        let mut now = INITIAL_RTO_NS;
        let mut next = None;
        while next.is_none() {
            next = detector.poll(now, &mut requests);
            now = detector.wake().unwrap().max(now);
        }
        let second = next.unwrap();
        assert_ne!(second.to, first.to, "the round's other member");
        assert_eq!(
            second.due_ns - detector.probe.unwrap().sent_ns,
            INITIAL_RTO_NS,
            "its own timer, not backed off"
        );
        let mut again = None;
        while again.is_none_or(|ping: Ping| ping.to != first.to) {
            again = detector.poll(now, &mut requests);
            now = detector.wake().unwrap().max(now);
        }
        assert_eq!(
            again.map(|ping| ping.due_ns - detector.probe.unwrap().sent_ns),
            Some(2 * INITIAL_RTO_NS),
            "the first pair's timer backed off: twice the initial second"
        );
    }

    /// A handshake is its pair's timer's first round trip (RFC 6298 (2.2)), as TCP's is: a peer
    /// joined with one is judged from that round trip, and one joined without from the initial
    /// second, all of it margin.
    #[test]
    fn a_handshake_is_its_pair_s_first_round_trip() {
        let mut detector =
            Detector::new(LOCAL, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        let handshake = Duration::from_micros(100);
        detector.join_measured(A, handshake).unwrap();
        detector.join(B).unwrap();
        let measured = detector.verdict(A).unwrap();
        assert_eq!(measured.round_trip, handshake, "the handshake's round trip");
        assert_eq!(detector.report(A).unwrap().timer.samples, 1);
        let unmeasured = detector.verdict(B).unwrap();
        assert_eq!(unmeasured.round_trip, Duration::ZERO, "none measured");
        assert_eq!(
            unmeasured.margin,
            Duration::from_nanos(INITIAL_RTO_NS),
            "the initial second, all margin"
        );
    }

    /// A pair's estimators follow round trips that lengthen. A first form ended a probe's period at
    /// its expected arrival from the latest round trip; when the round trips lengthened past what the
    /// outstanding probes cover (three a peer), every answer came for a probe written over, none was
    /// measured, and the member probed on at the stale pace and never configured: the cluster test's
    /// spin, a member 43,557 periods into a run with nothing judged while the others condemned it,
    /// slow to answer, 95 times. Each pair's timer waits for its own round trips and backs off where
    /// they outrun it, and each pair's own estimator configures from the lengthened ones.
    #[test]
    fn a_pair_s_estimators_follow_round_trips_that_lengthen() {
        let mut detector = detector(&[A, B]);
        let mut requests = Vec::new();
        // Answers in flight: when each arrives, from whom, for which probe.
        let mut flight: Vec<(u64, HostId, u64)> = Vec::new();
        let mut now = MS;
        let mut periods = 0;
        while periods < 2_000 {
            // A quick first round trip, then the load arrives: a hundred times as long.
            let rtt = if periods < 4 { MS / 20 } else { 5 * MS };
            if let Some(ping) = detector.poll(now, &mut requests) {
                periods += 1;
                flight.push((now + rtt, ping.to, ping.nonce));
            }
            let arrival = flight.iter().map(|(at, _, _)| *at).min();
            now = match (detector.wake().map(|wake| wake + MS / 100), arrival) {
                (Some(wake), Some(at)) => wake.min(at),
                (Some(wake), None) => wake,
                (None, Some(at)) => at,
                (None, None) => {
                    detector.on_ping(C);
                    now + MS
                }
            }
            .max(now);
            flight.retain(|&(at, from, nonce)| {
                let due = at <= now;
                if due {
                    detector.on_ack(from, nonce, at, detector.relay_within_ns(from));
                }
                !due
            });
        }
        assert!(
            [A, B].iter().all(|peer| detector
                .report(*peer)
                .is_some_and(|report| report.configured)),
            "configured from the lengthened round trips"
        );
    }

    /// A reconfiguration the estimator refuses leaves the verdict in force: under a CPU throttle a
    /// stall made `τ_int` unmeasured again, the verdict went, and the probe that would have
    /// suspected a crashed member was not judged (one Linux run in three hundred at one CPU).
    #[test]
    fn a_refused_reconfiguration_leaves_the_verdict_in_force() {
        let in_force = Verdict {
            round_trip: Duration::from_millis(1),
            margin: Duration::from_millis(2),
            interval: Duration::from_millis(30),
            loss: 0.01,
            mistake: 0.01,
        };
        let mut stream = Stream {
            verdict: Some(in_force),
            ..Stream::default()
        };
        let g = Duration::from_micros(50);
        stream.take(0, MS, g, Duration::from_millis(30));
        let refused = stream.estimator.as_ref().unwrap().behaviour();
        assert_eq!(refused.err(), Some(Refusal::TooFewHeartbeats));
        stream.configure(
            Some(Duration::from_secs(60)),
            g,
            Duration::from_millis(30),
            Duration::ZERO,
        );
        assert_eq!(stream.verdict, Some(in_force));
    }

    /// A suspicion re-adopted at a newer incarnation keeps the probes that already told the peer,
    /// and its pending condemnation: resetting them made a crashed member be told again from the
    /// start, past the detection bound (Linux at one CPU, where live members were suspected and
    /// refuted often).
    #[test]
    fn a_suspicion_adopted_again_keeps_its_told_probes() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 21;
        while detector.report(A).unwrap().pending_since_ns.is_none() {
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
            assert_ne!(liveness(&detector, A), Liveness::Dead, "pending first");
        }
        let incarnation = detector.membership().state(A).unwrap().incarnation;
        detector
            .apply(
                A,
                MemberState {
                    liveness: Liveness::Suspect,
                    incarnation: incarnation + 1,
                },
            )
            .unwrap();
        assert!(detector.report(A).unwrap().pending_since_ns.is_some());
    }

    /// Every probe is judged, so a member states its detection bound from its first period, and the
    /// bound covers its timers' deadlines: an unanswered timer-judged probe's period is its deadline
    /// and then its relays', three spans. A first form stated no bound while probes judged nothing,
    /// and a death the member held then was another's, adopted (the cluster test noted one 7 ms into
    /// a run against a bound of 2.5 ms its probes could not keep).
    #[test]
    fn a_member_states_its_bound_from_its_first_period() {
        let peers = [A, B];
        let mut detector = detector(&peers);
        let mut world = World::new();
        world.run(&mut detector, 2, |_| Some(MS));
        assert!(
            peers
                .iter()
                .all(|peer| detector.report(*peer).unwrap().judge == Judge::Timer)
        );
        let bound = detector.detection_bound(world.now).unwrap();
        // Two members watched: 2(2·2 − 1) + 1 periods, each up to three timer spans, a second each
        // before the pool has measured the host's stalls.
        let span = Duration::from_nanos(MINIMUM_RTO_NS);
        assert!(bound >= span * 3 * 7, "{bound:?}");
    }

    /// A member whose every wake is read exactly on time configures and judges: its `G` is the
    /// clock's resolution. As the mean of its wakes, zero, `G` floored nothing, and the member
    /// configured no pair and suspected nobody however long it ran (slates' harness, which wakes
    /// its detectors when they ask).
    #[test]
    fn a_member_whose_wakes_read_on_time_configures_and_judges() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        world.late = 0;
        configured(&mut detector, &mut world, &peers);
        assert_eq!(detector.granularity(), Some(RESOLUTION));
        let mut state = 21;
        while liveness(&detector, A) == Liveness::Alive {
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
        }
        assert_eq!(liveness(&detector, A), Liveness::Suspect);
    }

    /// A round trip measured before the member has a wake and a period measured is taken by no
    /// estimator, and counted.
    #[test]
    fn a_round_trip_before_the_first_wake_is_counted_unmeasured() {
        let mut detector = detector(&[A, B]);
        let mut requests = Vec::new();
        let first = detector.poll(0, &mut requests).unwrap();
        assert_eq!(detector.granularity(), None, "no wake yet");
        detector.on_ack(
            first.to,
            first.nonce,
            MS,
            detector.relay_within_ns(first.to),
        );
        assert_eq!(detector.unmeasured(), 1);
        assert_eq!(detector.round_trips_taken(first.to), Some(0));
    }

    /// The detection bound counts every member the view has held, not the current round's: a member
    /// that condemned two others (falsely, under a CPU throttle) ran rounds of one while the
    /// victim's last probe had been in a round of three, and stated a bound a third as long. Its
    /// periods' multiplier is checked exactly; their length follows the verdicts judging the pairs
    /// probed now, and the periods already run.
    #[test]
    fn the_detection_bound_does_not_shrink_with_the_round() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        // The periods the bound counts: 2(2m − 1) + 1, `m` the most members the view has held.
        let periods = |detector: &Detector, now: u64| {
            let bound = nanos(detector.detection_bound(now).unwrap());
            bound / detector.period_bound(now).unwrap()
        };
        let before = periods(&detector, world.now);
        assert_eq!(before, 2 * (2 * 3 - 1) + 1);
        for peer in [B, C] {
            detector
                .apply(
                    peer,
                    MemberState {
                        liveness: Liveness::Dead,
                        incarnation: 0,
                    },
                )
                .unwrap();
        }
        let mut state = 17;
        world.run(&mut detector, 4, |_| Some(jitter(&mut state)));
        assert_eq!(detector.order.len(), 1, "a round of one");
        assert_eq!(periods(&detector, world.now), before);
        // Nor once the dead are forgotten: they were in the rounds before.
        while detector.membership().len() > 2 {
            world.run(&mut detector, 1, |_| Some(jitter(&mut state)));
        }
        assert_eq!(periods(&detector, world.now), before);
    }

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

    /// Gossip naming more members than the view holds is refused past its bound, typed and
    /// counted, and nothing keyed by a member outgrows the view.
    #[test]
    fn gossip_of_more_members_than_the_view_holds_is_refused() {
        let mut detector = Detector::new(
            LOCAL,
            Exposure::new(),
            NonZeroUsize::new(8).unwrap(),
            RESOLUTION,
            Duration::ZERO,
        );
        detector.apply_gossip((10..10_010u64).map(|id| (HostId(id), alive(0))));
        assert_eq!(
            detector.membership().len(),
            8,
            "the bound, this one included"
        );
        assert_eq!(detector.refused(), 10_000 - 7);
        assert_eq!(detector.apply(HostId(1 << 40), alive(0)), Err(Full));
        assert_eq!(
            detector.apply(HostId(1 << 40), dead(0)),
            Ok(None),
            "a death of a member not held changes nothing"
        );
        assert_eq!(detector.join(HostId(1 << 41)), Err(Full));
        assert!(detector.peers.len() <= 7);
        assert!(detector.gossip.pending() <= 7);
    }

    /// A dead member's record is kept for the window in which gossip of it from before its death
    /// can still arrive, and refuses that gossip; then it is forgotten, with everything held of
    /// it, and a later incarnation's alive, a live member refuting, is a member again.
    #[test]
    fn a_dead_member_is_forgotten_once_its_window_has_passed() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 23;
        while liveness(&detector, A) != Liveness::Dead {
            world.run(&mut detector, 1, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
        }
        let died = detector.membership().state(A).unwrap().incarnation;
        let stamped = loop {
            if let Some(at) = detector.deaths.back().and_then(|death| death.at_ns) {
                break at;
            }
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
        };
        let window = detector.dissemination_window_ns(world.now).unwrap();
        while detector.membership().state(A).is_some() {
            assert_eq!(
                detector.apply(A, alive(died)),
                Ok(None),
                "gossip from before the death is refused by its record"
            );
            world.run(&mut detector, 1, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
        }
        assert!(world.now - stamped >= window, "kept for its window");
        assert_eq!(detector.report(A), None);
        assert_eq!(detector.predicted_rtt(A), None);
        assert!(!detector.peers.contains_key(&A));
        assert!(detector.apply(A, alive(died + 1)).unwrap().is_some());
        assert_eq!(liveness(&detector, A), Liveness::Alive, "a member again");
    }

    /// A member with nobody alive or suspected left keeps its dead past their windows: they are
    /// the members it probes, and a live one among them refutes.
    #[test]
    fn an_isolated_member_keeps_its_dead() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        for peer in peers {
            let incarnation = detector.membership().state(peer).unwrap().incarnation;
            detector.apply(peer, dead(incarnation)).unwrap();
        }
        world.run(&mut detector, 1, |_| None);
        let stamped = detector
            .deaths
            .back()
            .and_then(|death| death.at_ns)
            .unwrap();
        while world.now - stamped <= 2 * detector.dissemination_window_ns(world.now).unwrap() {
            world.run(&mut detector, 1, |_| None);
        }
        for peer in peers {
            assert_eq!(liveness(&detector, peer), Liveness::Dead, "{peer:?}");
        }
        let last = world.pings.last().unwrap().to;
        assert!(peers.contains(&last), "it probes its dead");
    }

    /// At its bound, a member takes a newcomer in place of a dead member's record past its window,
    /// and refuses one while the records are inside theirs.
    #[test]
    fn a_record_past_its_window_makes_room_and_one_inside_it_does_not() {
        let peers = [A, B, C];
        let mut detector = Detector::new(
            LOCAL,
            Exposure::new(),
            NonZeroUsize::new(4).unwrap(),
            RESOLUTION,
            Duration::ZERO,
        );
        for peer in peers {
            detector.join(peer).unwrap();
        }
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        for peer in peers {
            let incarnation = detector.membership().state(peer).unwrap().incarnation;
            detector.apply(peer, dead(incarnation)).unwrap();
        }
        let newcomer = HostId(9);
        assert_eq!(detector.apply(newcomer, alive(0)), Err(Full), "inside");
        assert_eq!(detector.refused(), 1);
        // Isolated, it forgets none of its dead on its own; the newcomer takes the oldest's place.
        world.run(&mut detector, 1, |_| None);
        let stamped = detector
            .deaths
            .back()
            .and_then(|death| death.at_ns)
            .unwrap();
        while world.now - stamped <= detector.dissemination_window_ns(world.now).unwrap() {
            world.run(&mut detector, 1, |_| None);
        }
        assert_eq!(detector.membership().len(), 4);
        assert!(detector.apply(newcomer, alive(0)).unwrap().is_some());
        assert_eq!(liveness(&detector, newcomer), Liveness::Alive);
        assert_eq!(detector.membership().len(), 4, "one record gave its place");
        assert_eq!(detector.refused(), 1);
    }

    /// A pair no estimator judges is judged by its own timer from its first probe. Before
    /// 2026-10-10 a probe was measurement only until an estimator configured, and a peer silent
    /// before one did was judged by nothing (slates: 4,000 periods); a first restatement judged such
    /// a pair only once silent for the timeout, which paid the timeout twice. Do: run a member over
    /// A, which never answers, and B, which answers in about 1 ms. Expect: A's first probe is judged
    /// at RFC 6298's initial second (2.1), by a verdict that claims no bound, and A is suspected when
    /// that probe's period ends unanswered, not before its deadline.
    #[test]
    fn a_pair_no_estimator_judges_is_judged_by_its_timer() {
        let mut detector = detector(&[A, B]);
        let mut world = World::new();
        world.heard = B;
        let mut state = 7;
        while !world.pings.iter().any(|ping| ping.to == A) {
            world.run(&mut detector, 0, |peer| {
                (peer == B).then(|| jitter(&mut state))
            });
        }
        let probe = detector.probe.unwrap();
        assert_eq!(probe.target, A);
        assert_eq!(probe.verdict.mistake, 1.0, "the timer's: no bound claimed");
        assert_eq!(
            probe.due_ns() - probe.sent_ns,
            INITIAL_RTO_NS,
            "the initial second"
        );
        // The period's end: the next probe goes out, and A's first probe has been judged.
        let sent = world.pings.len();
        while world.pings.len() == sent {
            assert_eq!(
                liveness(&detector, A),
                Liveness::Alive,
                "not before its deadline"
            );
            world.run(&mut detector, 0, |peer| {
                (peer == B).then(|| jitter(&mut state))
            });
        }
        assert!(world.now >= probe.due_ns(), "past its deadline");
        assert_eq!(
            liveness(&detector, A),
            Liveness::Suspect,
            "suspected when its first probe's period ended"
        );
    }

    #[test]
    fn the_deadline_is_the_mean_round_trip_plus_the_margin() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let ping = detector.poll(world.now, &mut world.requests);
        let ping = ping.or_else(|| {
            world.now = detector.wake().unwrap();
            detector.on_ack(
                world.pings.last().unwrap().to,
                world.pings.last().unwrap().nonce,
                world.now,
                detector.relay_within_ns(world.pings.last().unwrap().to),
            );
            detector.poll(world.now, &mut world.requests)
        });
        let ping = ping.unwrap();
        let verdict = detector.verdict(ping.to).unwrap();
        assert_eq!(detector.wake(), Some(world.now + verdict.span_ns()));
        assert!(verdict.round_trip >= Duration::from_millis(1));
        assert!(verdict.round_trip <= Duration::from_micros(1_500));
        assert!(verdict.mistake > 0.0 && verdict.mistake < 1.0);
        // G is the lateness of the wakes: the world's, or less where an answer woke it first.
        let g = detector.granularity().unwrap();
        assert!(g > Duration::ZERO && g <= Duration::from_nanos(world.late));
        // The pair's next probe goes no sooner than this one's deadline: its probes never overlap,
        // so the bound on one missing holds at any margin (`verdict`).
        let due = ping.due_ns;
        let sent = world.pings.len();
        let mut state = 5;
        // One period at a time, so the latest probe is the first of the pair's after this one.
        while !world.pings[sent..].iter().any(|next| next.to == ping.to) {
            world.run(&mut detector, 0, |_| Some(jitter(&mut state)));
        }
        let next = detector.probe.unwrap().sent_ns;
        assert!(
            next >= due,
            "the pair's next probe went at {next}, before {due}"
        );
    }

    #[test]
    fn a_silent_member_is_suspected_told_and_condemned() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 11;
        let mut suspected_first = false;
        for _ in 0..200 {
            world.run(&mut detector, 1, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
            match liveness(&detector, A) {
                Liveness::Suspect => suspected_first = true,
                Liveness::Dead => break,
                Liveness::Alive => {}
            }
        }
        assert!(suspected_first, "suspected before condemned");
        assert_eq!(liveness(&detector, A), Liveness::Dead);
        let report = detector.report(A).unwrap();
        assert_eq!((report.suspicions, report.condemnations), (1, 1));
        assert!(report.condemned_after.unwrap() <= report.condemned_within.unwrap());
        assert!(
            world.asked.iter().any(|r| r.target == A && r.relay != A),
            "relays were asked before the suspicion"
        );
        for peer in [B, C] {
            assert_eq!(liveness(&detector, peer), Liveness::Alive);
        }
    }

    /// Drives `detector` from `now` with any number of answers in flight: `answer` gives each probe's
    /// round trip (none for silence); `heard` is called with each answer's sender, nonce and arrival as
    /// it is delivered, before the detector sees it, and stops the run by returning true; the run also
    /// stops at `horizon` or when `done` holds. Returns the time it stopped.
    fn drive_many(
        detector: &mut Detector,
        mut now: u64,
        horizon: u64,
        mut answer: impl FnMut(HostId) -> Option<u64>,
        mut heard: impl FnMut(&Detector, HostId, u64, u64) -> bool,
        done: impl Fn(&Detector) -> bool,
    ) -> u64 {
        let mut requests = Vec::new();
        let mut pending: Vec<(HostId, u64, u64)> = Vec::new();
        while now < horizon && !done(detector) {
            let ping = detector.poll(now, &mut requests);
            detector.check_extremes();
            if let Some(ping) = ping
                && let Some(rtt) = answer(ping.to)
            {
                pending.push((ping.to, ping.nonce, now + rtt));
            }
            let ack = pending.iter().map(|(_, _, at)| *at).min();
            now = match (detector.wake().map(|wake| wake + MS / 10), ack) {
                (Some(wake), Some(at)) => wake.min(at).max(now),
                (Some(wake), None) => wake.max(now),
                (None, Some(at)) => at.max(now),
                (None, None) => {
                    detector.on_ping(A);
                    now + MS
                }
            };
            pending.sort_by_key(|(_, _, at)| *at);
            let due: Vec<(HostId, u64, u64)> = pending
                .iter()
                .copied()
                .filter(|(_, _, at)| *at <= now)
                .collect();
            pending.retain(|(_, _, at)| *at > now);
            for (from, nonce, at) in due {
                if heard(detector, from, nonce, at) {
                    return now;
                }
                detector.on_ack(from, nonce, at, detector.relay_within_ns(from));
                detector.check_extremes();
            }
        }
        now
    }

    /// The detection budget the period-floor tests state: slates' membership horizon (slates
    /// `lease::horizon_ns`, its A-125), the budget the first owner to state one states.
    const BUDGET: Duration = Duration::from_millis(900);

    /// A member over `peers` judged under [`BUDGET`].
    fn budgeted(peers: &[HostId]) -> Detector {
        let mut detector = Detector::new(LOCAL, Exposure::new(), room(), RESOLUTION, BUDGET);
        for &peer in peers {
            detector.join(peer).unwrap();
        }
        detector
    }

    /// Whether every one of `peers` is judged by a verdict whose period rests at the floor: the
    /// pair's own or the pool's, not its timer's.
    fn judged_at_floor(detector: &Detector, peers: &[HostId]) -> bool {
        peers.iter().all(|peer| {
            detector.peers.get(peer).is_some_and(|held| {
                !judging(
                    held,
                    detector.pool.verdict,
                    detector.probe_loss(),
                    detector.granularity_ns(),
                )
                .1
            })
        })
    }

    /// The period floor (slates, 2026-10-09: an idle two-member pair probed about 1,700 times a
    /// second per member on loopback, each period ending at its probe's deadline). Do: run a member
    /// over three near peers, every probe answered in about 1 ms, until every pair is judged by a
    /// verdict that rests (the pool's or its own), then count its probes for one detection budget,
    /// under a 900 ms budget and under none. Expect: under the budget at most one probe per floor,
    /// `D / (2(2m − 1) + 1)` with `m` = 3, and at least one; with none, more than that, at its round
    /// trip's pace. Before the floor (the budget taken and unused) the budgeted member probed as
    /// often as the unbudgeted one.
    #[test]
    fn an_idle_member_probes_no_faster_than_its_detection_budget_needs() {
        const HORIZON: u64 = 600_000 * MS;
        let peers = [A, B, C];
        let budget = nanos(BUDGET);
        let floor = budget / (2 * (2 * 3 - 1) + 1);
        let at_most = budget / floor + 1;
        let probes = |mut detector: Detector| {
            let mut state = 0x2545_F491_4F6C_DD1D;
            let judged = drive_many(
                &mut detector,
                MS,
                HORIZON,
                |_| Some(jitter(&mut state)),
                |_, _, _, _| false,
                |detector| judged_at_floor(detector, &peers),
            );
            assert!(judged_at_floor(&detector, &peers), "every pair judged");
            let mut pings = 0u64;
            drive_many(
                &mut detector,
                judged,
                judged + budget,
                |_| {
                    pings += 1;
                    Some(jitter(&mut state))
                },
                |_, _, _, _| false,
                |_| false,
            );
            pings
        };
        let under_budget = probes(budgeted(&peers));
        assert!(
            (1..=at_most).contains(&under_budget),
            "{under_budget} probes in one budget, at most one per {floor} ns floor and at least one"
        );
        let unbudgeted = probes(detector(&peers));
        assert!(
            unbudgeted > at_most,
            "with no budget the member probes at its round trip: {unbudgeted} probes"
        );
    }

    /// A budgeted member over three near peers, every pair judged at its floor and every peer's
    /// relay bound stated since, and when: two rounds more, so each peer has answered after a
    /// period began with every pair judged, and states a bound drawn from verdicts, not from the
    /// timers before them.
    fn judged_under_budget() -> (Detector, u64) {
        let peers = [A, B, C];
        let mut detector = budgeted(&peers);
        let mut state = 0x2545_F491_4F6C_DD1D;
        let judged = drive_many(
            &mut detector,
            MS,
            600_000 * MS,
            |_| Some(jitter(&mut state)),
            |_, _, _, _| false,
            |detector| judged_at_floor(detector, &peers),
        );
        assert!(judged_at_floor(&detector, &peers), "every pair judged");
        let periods = detector.periods.count + 2 * 3;
        let now = drive_many(
            &mut detector,
            judged,
            judged + 600_000 * MS,
            |_| Some(jitter(&mut state)),
            |_, _, _, _| false,
            |detector| detector.periods.count >= periods,
        );
        (detector, now)
    }

    /// Polls from `now` until the next period starts: its probe.
    fn next_probe(detector: &mut Detector, now: &mut u64) -> Ping {
        let mut requests = Vec::new();
        loop {
            if let Some(ping) = detector.poll(*now, &mut requests) {
                return ping;
            }
            *now = detector.wake().unwrap().max(*now);
        }
    }

    /// An owner re-derives its budget as its measurements change (slates: its failover objective
    /// less the council's window for confirming a death): a new budget floors the next period, and a
    /// period resting now keeps the rest its start set.
    #[test]
    fn a_new_detection_budget_floors_the_next_period_and_a_resting_one_keeps_its_rest() {
        let (mut detector, mut now) = judged_under_budget();
        let floor = |budget: Duration| nanos(budget) / (2 * (2 * 3 - 1) + 1);
        let ping = next_probe(&mut detector, &mut now);
        detector.on_ack(
            ping.to,
            ping.nonce,
            now + MS,
            detector.relay_within_ns(ping.to),
        );
        let resting = detector.probe.unwrap();
        assert!(!resting.provisional, "judged by an estimator");
        assert_eq!(resting.rests_until, resting.sent_ns + floor(BUDGET));
        detector.set_detection_budget(BUDGET / 3);
        assert_eq!(
            detector.probe.unwrap().rests_until,
            resting.rests_until,
            "the period resting keeps its rest"
        );
        assert_eq!(detector.wake(), Some(resting.rests_until));
        now = resting.rests_until;
        let next = next_probe(&mut detector, &mut now);
        assert_ne!(next.nonce, ping.nonce);
        let started = detector.probe.unwrap();
        assert_eq!(started.rests_until, started.sent_ns + floor(BUDGET / 3));
    }

    /// The least budget the member's deadlines support is its detection bound with every period at
    /// what an unanswered probe's deadlines allow, not at the lengths the budget's floor stretched
    /// its periods to; and a budget set at it floors a period at exactly that. Nothing before a
    /// period.
    #[test]
    fn the_least_detection_bound_is_the_bound_the_deadlines_allow_without_the_floor() {
        assert_eq!(budgeted(&[A, B, C]).least_detection_bound(MS), None);
        let (mut detector, now) = judged_under_budget();
        let least = detector.least_detection_bound(now).unwrap();
        let period = detector.longest_unanswered + detector.wakes.latest_ns(now);
        assert_eq!(least, Duration::from_nanos(period * (2 * (2 * 3 - 1) + 1)));
        let bound = detector.detection_bound(now).unwrap();
        assert!(least < bound, "{least:?} under the floor's {bound:?}");
        detector.set_detection_budget(least);
        assert_eq!(detector.period_floor_ns(), period);
    }

    /// Only a period an estimator judges rests: a timer-judged period ends at its answer whatever the
    /// budget, as the samples its pair's estimator is still gathering need (slates, 2026-10-09: when
    /// every period rested, a lone peer was still alive in its member's view 9 s into its silence).
    /// Do: under a 900 ms budget, answer a fresh member's first probe 1 ms after it is sent. Expect:
    /// the next probe goes at that answer, not at the floor.
    #[test]
    fn a_timer_judged_period_ends_at_its_answer_under_a_budget() {
        let mut detector = budgeted(&[A, B]);
        let mut requests = Vec::new();
        let first = detector.poll(0, &mut requests).unwrap();
        assert_eq!(
            first.due_ns, INITIAL_RTO_NS,
            "judged by its timer, the initial second"
        );
        detector.on_ack(
            first.to,
            first.nonce,
            MS,
            detector.relay_within_ns(first.to),
        );
        assert_eq!(detector.wake(), Some(0), "nothing left to wait for");
        assert!(
            detector.poll(MS, &mut requests).is_some(),
            "the next probe goes at the answer"
        );
    }

    /// A fleet whose nodes fail once an hour, as `docs/timing.md` §3 item 11 seeds one: a thousand
    /// failures over a thousand hours of node time.
    fn hourly_fleet() -> Exposure {
        let mut history = Exposure::new();
        history.on_exposure(Duration::from_secs(3_600 * 1_000));
        for _ in 0..1_000 {
            history.on_failure();
        }
        history
    }

    /// A member over A, B and C seeded with [`hourly_fleet`], run until its pool first configures,
    /// everyone answering in about 1 ms; with the pool's estimator, the member's `G`, the pair
    /// interval and the MTBF as they stand.
    fn pooled_at_first() -> (Detector, Duration, Duration, Duration) {
        let mut detector = Detector::new(LOCAL, hourly_fleet(), room(), RESOLUTION, Duration::ZERO);
        for peer in [A, B, C] {
            detector.join(peer).unwrap();
        }
        let mut state = 0x2545_F491_4F6C_DD1D;
        drive_many(
            &mut detector,
            MS,
            600_000 * MS,
            |_| Some(jitter(&mut state)),
            |_, _, _, _| false,
            |detector| detector.pool.verdict.is_some(),
        );
        let granularity = detector.granularity().unwrap();
        let interval = detector.pair_interval(detector.periods.mean_ns().unwrap());
        let mtbf = detector.exposure.mtbf().unwrap();
        (detector, granularity, interval, mtbf)
    }

    /// `U` (§2.2) of a probe verdict at margin `alpha`, seconds: a crash detected within
    /// `μ + α + η` and then the cost `cost`, once an MTBF, and a mistake costing `cost`, at most one
    /// a probe of the pair, with chance at most `β = p + (1 − p)·V/(V + α²)`.
    fn unavailability(link: &LinkBehaviour, mtbf: f64, cost: f64, eta: f64, alpha: f64) -> f64 {
        let mean = link.mean_delay.as_secs_f64();
        let variance = link.delay_deviation.as_secs_f64().powi(2);
        let beta = if alpha > 0.0 {
            link.loss + (1.0 - link.loss) * variance / (variance + alpha * alpha)
        } else {
            1.0
        };
        (mean + alpha + eta + cost) / mtbf + cost * beta / eta
    }

    /// A verdict's margin is the one minimizing `U` over every margin (slates, 2026-10-10: a verdict
    /// with mistake bound 0.713 was in force, its margin held at `η − G`, 125 µs, by the cap that
    /// kept Theorem 7's product to one factor; a pair's probes never overlap, so one factor holds at
    /// any margin). Do: seed a member with a fleet failing once an hour, run it until its pool
    /// first configures, and configure a verdict from the pool's estimates as they stand. Expect: no
    /// margin a granularity wider costs less. Before, the margin was held under the interval while
    /// `U` still fell past it.
    #[test]
    fn a_verdict_s_margin_is_not_held_under_its_interval() {
        let (detector, granularity, interval, mtbf) = pooled_at_first();
        let estimator = detector.pool.estimator.as_deref().unwrap();
        let configured =
            verdict(estimator, Some(mtbf), granularity, interval, Duration::ZERO).unwrap();
        let link = estimator.behaviour().unwrap();
        let eta = interval.as_secs_f64();
        let cost = eta + link.mean_delay.as_secs_f64();
        let u = |alpha: f64| unavailability(&link, mtbf.as_secs_f64(), cost, eta, alpha);
        let alpha = configured.margin.as_secs_f64();
        let wider = alpha + granularity.as_secs_f64();
        assert!(
            u(wider) >= u(alpha),
            "a margin {granularity:?} wider costs less: U({wider}) = {} < U({alpha}) = {}, \
             interval {interval:?}",
            u(wider),
            u(alpha)
        );
        assert!(
            configured.margin > interval,
            "an hourly fleet's margin is past one interval: {configured:?}"
        );
    }

    /// A stated condemnation cost widens the margin: a false condemnation costs the owner its
    /// response to a death, and a live peer's two probes both miss with chance at most the lesser of
    /// their bounds (Fréchet), so the verdict charges each probe that cost at its bound. Do: from
    /// the same estimates as above, configure with no condemnation cost and with one equal to a
    /// false suspicion's own cost (`η + μ`, so a mistake costs twice as much). Expect: the priced
    /// verdict's margin is wider and its bound lower. Before, the cost was not priced: the same
    /// verdict.
    #[test]
    fn a_stated_condemnation_cost_widens_the_margin() {
        let (detector, granularity, interval, mtbf) = pooled_at_first();
        let estimator = detector.pool.estimator.as_deref().unwrap();
        let link = estimator.behaviour().unwrap();
        let suspicion = interval.saturating_add(link.mean_delay);
        let unpriced =
            verdict(estimator, Some(mtbf), granularity, interval, Duration::ZERO).unwrap();
        let priced = verdict(estimator, Some(mtbf), granularity, interval, suspicion).unwrap();
        assert!(
            priced.margin > unpriced.margin && priced.mistake < unpriced.mistake,
            "priced {priced:?} against unpriced {unpriced:?}"
        );
    }

    /// A pair one stall made late is the pool's to judge again from its next answer in time, and
    /// feeds it again from the sample after (slates, 2026-10-10: on a host at load 80 an answer after
    /// a stall marked its pair a misfit for good; both of a member's pairs were marked so, nothing fed
    /// its pool, which refused at 5 round trips, and a peer that then went silent was judged by
    /// nothing for 4,000 periods). Do: on a member over A and B whose pool judges them, answering in
    /// about 1 ms, answer one probe of B's 40 ms late, past the pool's span, then every probe in
    /// about 1 ms. Expect: at the late answer B leaves the pool's judgment for its timer's; at its
    /// next answer in time it returns to the pool's; and its answer after that feeds the pool, the
    /// sample judged by the evidence before it. Before, a mark set once kept B out of both until its
    /// own estimator configured.
    #[test]
    fn a_pair_one_stall_made_late_returns_to_the_pool() {
        const STALL_RTT: u64 = 40 * MS;
        let mut detector = detector(&[A, B]);
        let mut state = 0x9E37_79B9_7F4A_7C15;
        let judge = |detector: &Detector, peer: HostId| detector.report(peer).unwrap().judge;
        let mut now = drive_many(
            &mut detector,
            MS,
            600_000 * MS,
            |_| Some(jitter(&mut state)),
            |_, _, _, _| false,
            |detector| {
                [A, B]
                    .iter()
                    .all(|peer| judge(detector, *peer) == Judge::Pool)
            },
        );
        assert!(
            [A, B]
                .iter()
                .all(|peer| judge(&detector, *peer) == Judge::Pool)
        );
        let mut requests = Vec::new();
        let mut pending: Vec<(HostId, u64, u64)> = Vec::new();
        // The stalled probe's nonce, then B's answers after its late one: none yet, one, two.
        let mut stalled = None;
        let (mut left, mut after) = (false, 0);
        while after < 2 {
            if let Some(ping) = detector.poll(now, &mut requests) {
                let rtt = if ping.to == B && stalled.is_none() {
                    stalled = Some(ping.nonce);
                    STALL_RTT
                } else {
                    jitter(&mut state)
                };
                pending.push((ping.to, ping.nonce, now + rtt));
            }
            let ack = pending.iter().map(|(_, _, at)| *at).min();
            now = match (detector.wake(), ack) {
                (Some(wake), Some(at)) => wake.min(at).max(now),
                (Some(wake), None) => wake.max(now),
                (None, Some(at)) => at.max(now),
                (None, None) => now + MS,
            };
            pending.sort_by_key(|(_, _, at)| *at);
            for (from, nonce, at) in pending.iter().copied().filter(|(_, _, at)| *at <= now) {
                let samples = detector.pool.samples;
                detector.on_ack(from, nonce, at, detector.relay_within_ns(from));
                if from != B {
                    continue;
                }
                if Some(nonce) == stalled {
                    left = true;
                    assert_eq!(judge(&detector, B), Judge::Timer, "left at the late answer");
                } else if left && after == 0 {
                    after = 1;
                    assert_eq!(judge(&detector, B), Judge::Pool, "back at its next answer");
                    assert_eq!(detector.pool.samples, samples, "its far evidence before it");
                } else if left {
                    after = 2;
                    assert_eq!(
                        detector.pool.samples,
                        samples + 1,
                        "the answer after feeds the pool"
                    );
                }
            }
            pending.retain(|(_, _, at)| *at > now);
            assert!(now < 600_000 * MS, "the stall's sequence ran");
        }
    }

    /// An answer no probe of the pair explains is evidence of life, not of the pair's path (slates,
    /// 2026-10-09: an owner renewed its lease holders with pings of its own on the same plane, their
    /// nonces from 2⁶³; every pair that answered one before configuring was marked a pool misfit for
    /// good, the pool was never fed, and a member that then died was measured for ever and never
    /// suspected, 30 s into its silence, in 3 of 11 warm-restart runs). Do: on a fresh member over A
    /// and B, answer each of their first probes with an extra acknowledgement carrying a nonce far
    /// outside the member's own, then answer everything until the pool configures, and silence A.
    /// Expect: neither pair's round trips were kept from the pool, the pool configures, and A is
    /// suspected.
    #[test]
    fn an_answer_no_probe_explains_moves_nothing_and_a_silent_peer_is_suspected() {
        /// Shape: a nonce from a range no probe of the member reaches (its own count from zero; a
        /// lease renewal's from 2⁶³).
        const FOREIGN: u64 = 1 << 63;
        let mut detector = detector(&[A, B]);
        let mut state = 0x2545_F491_4F6C_DD1D;
        let mut requests = Vec::new();
        let mut now = MS;
        for _ in 0..4 {
            if let Some(ping) = detector.poll(now, &mut requests) {
                now += jitter(&mut state);
                detector.on_ack(ping.to, FOREIGN, now, detector.relay_within_ns(ping.to));
                detector.on_ack(ping.to, ping.nonce, now, detector.relay_within_ns(ping.to));
            }
            now += MS;
        }
        for peer in [A, B] {
            assert!(
                !detector.report(peer).unwrap().pool_misfit,
                "an answer no probe explains moves nothing: {peer:?}"
            );
        }
        let configured = drive_many(
            &mut detector,
            now,
            now + 600_000 * MS,
            |_| Some(jitter(&mut state)),
            |_, _, _, _| false,
            |detector| detector.pool.verdict.is_some(),
        );
        assert!(detector.pool.verdict.is_some(), "the pool configured");
        drive_many(
            &mut detector,
            configured,
            configured + 30_000 * MS,
            |peer| (peer != A).then(|| jitter(&mut state)),
            |_, _, _, _| false,
            |detector| liveness(detector, A) != Liveness::Alive,
        );
        assert_ne!(
            liveness(&detector, A),
            Liveness::Alive,
            "the silent peer is suspected"
        );
    }

    /// A death counts in the failure history once it stands (slates, 2026-10-10: every adopted
    /// death counted, the false and refuted ones too, so a false condemnation shortened the MTBF
    /// the margins are priced with, and the next verdicts were the more aggressive for it). Do: on
    /// a configured member over A, B and C, adopt A's death and then its refutation; then adopt C's
    /// death and run until its record is forgotten past its window; then adopt C's older suspicion,
    /// as a member the death had not reached passes it on, and the death again, until forgotten
    /// again. Expect: no failure after the refutation, and one once C is forgotten, the same after
    /// it comes back and goes again. Before, the refuted death counted.
    #[test]
    fn a_death_counts_once_it_stands() {
        let (mut detector, now) = warmed();
        let dead = MemberState {
            liveness: Liveness::Dead,
            incarnation: 0,
        };
        detector.apply(A, dead).unwrap();
        detector.apply(A, alive(1)).unwrap();
        assert_eq!(
            detector.exposure().failures(),
            0,
            "a refuted death is no failure"
        );
        detector.apply(C, dead).unwrap();
        assert_eq!(detector.exposure().failures(), 0, "not yet stood");
        let mut state = 3;
        drive_many(
            &mut detector,
            now,
            now + 600_000 * MS,
            |peer| (peer != C).then(|| jitter(&mut state)),
            |_, _, _, _| false,
            |detector| detector.membership().state(C).is_none(),
        );
        assert_eq!(detector.membership().state(C), None, "C forgotten");
        assert_eq!(detector.exposure().failures(), 1, "the death that stood");
        // A member the death had not reached passes on its older state of C: adopted as a
        // newcomer's, condemned again, forgotten again. The same death, counted once.
        let suspected = MemberState {
            liveness: Liveness::Suspect,
            incarnation: 0,
        };
        detector.apply(C, suspected).unwrap();
        detector.apply(C, dead).unwrap();
        let again = detector.clock_ns;
        drive_many(
            &mut detector,
            again,
            again + 600_000 * MS,
            |peer| (peer != C).then(|| jitter(&mut state)),
            |_, _, _, _| false,
            |detector| detector.membership().state(C).is_none(),
        );
        assert_eq!(detector.membership().state(C), None, "C forgotten again");
        assert_eq!(detector.exposure().failures(), 1, "one death, counted once");
    }

    /// A peer silent before its member's pool configures is still judged (slates, 2026-10-10: a
    /// peer whose pair and pool estimators both refused was judged by nothing for 4,000 periods).
    /// Do: on a fresh member of a two-member view, answer its peer's first five probes in about
    /// 1 ms, far fewer than any estimator configures on, then nothing. Expect: the peer is
    /// suspected, by its pair's timer, and never condemned (a lone pair has no third member to
    /// confirm a death). Before, its probes stayed measurement only: alive after 600 s simulated.
    #[test]
    fn a_peer_silent_before_any_estimate_configures_is_suspected() {
        const ANSWERED: u32 = 5;
        let mut detector = detector(&[A]);
        let mut state = 0x2545_F491_4F6C_DD1D;
        let mut answered = 0u32;
        drive_many(
            &mut detector,
            MS,
            600_000 * MS,
            |_| {
                answered += 1;
                (answered <= ANSWERED).then(|| jitter(&mut state))
            },
            |_, _, _, _| false,
            |detector| liveness(detector, A) == Liveness::Suspect,
        );
        assert_eq!(detector.pool.verdict, None, "no estimate configured");
        assert_eq!(
            liveness(&detector, A),
            Liveness::Suspect,
            "the silent peer is suspected"
        );
        assert_eq!(
            detector.report(A).unwrap().judge,
            Judge::Timer,
            "by its pair's timer"
        );
        let suspected = detector.clock_ns;
        drive_many(
            &mut detector,
            suspected,
            suspected + 60_000 * MS,
            |_| None,
            |_, _, _, _| false,
            |_| false,
        );
        assert_ne!(
            liveness(&detector, A),
            Liveness::Dead,
            "a lone pair never condemns"
        );
    }

    /// A configured member over near peers A, B and C, and the time its warm-up ended: the member a
    /// far or late peer then joins.
    fn warmed() -> (Detector, u64) {
        let mut detector = detector(&[A, B, C]);
        let mut world = World::new();
        configured(&mut detector, &mut world, &[A, B, C]);
        (detector, world.now)
    }

    /// RFC 6298 §2.1 for a peer joined with no handshake round trip that has not answered yet: it is
    /// judged provisionally at the initial RTO, not by the pool. Do: on a configured member, join a far
    /// peer with no handshake round trip that answers in 200 ms, and run until its first answer
    /// arrives, then on for 30 s. Expect: it was not suspected before that answer, and never condemned.
    /// Before: the pool judged it at a few milliseconds, and the live peer was suspected before its
    /// first answer came back (132 ms in, on the driver of the test below).
    #[test]
    fn a_peer_joined_without_a_round_trip_is_not_suspected_before_its_first_answer() {
        const F: HostId = HostId(5);
        let (mut detector, now) = warmed();
        detector.join(F).unwrap();
        let mut state = 0x9E37_79B9_7F4A_7C15;
        let mut suspected_before_first_answer = None;
        drive_many(
            &mut detector,
            now,
            now + 30_000 * MS,
            |peer| {
                Some(if peer == F {
                    200 * MS
                } else {
                    jitter(&mut state)
                })
            },
            |detector, from, _, _| {
                if from == F && suspected_before_first_answer.is_none() {
                    // Held, alive, with its report: a member condemned and forgotten has neither.
                    suspected_before_first_answer = Some((
                        detector.membership().state(F).map(|state| state.liveness),
                        detector.report(F).map(|report| report.suspicions),
                    ));
                }
                false
            },
            |_| false,
        );
        assert_eq!(
            suspected_before_first_answer,
            Some((Some(Liveness::Alive), Some(0))),
            "held alive and never suspected when its first answer arrived"
        );
        let report = detector.report(F).unwrap_or_default();
        assert_eq!(report.condemnations, 0, "never condemned while alive");
        assert_eq!(
            detector.membership().state(F).map(|state| state.liveness),
            Some(Liveness::Alive),
            "held alive"
        );
    }

    /// The cost of RFC 6298 (2.1)'s initial timeout for a near peer joined with no handshake round
    /// trip: until its first answer its timer judges it at the initial second, not the pool. A
    /// condemnation takes two judged probes (the one that suspects and the one that tells), each
    /// unanswered one waiting out its direct deadline and then the relays', which includes the
    /// target's own span; the told probe's timer has backed off once (5.5). So the first detection
    /// of a peer that never answers moves by at most `I + I` and `2I + 2I`, six initial timeouts.
    /// Do: on a configured member, join a near peer that never answers, once with no handshake round
    /// trip and once with a 1 ms one, and time each from the join to holding it dead. Expect: both
    /// held dead, the first no more than six initial timeouts after the second.
    #[test]
    fn a_near_peer_joined_without_a_round_trip_is_condemned_at_most_six_initial_timeouts_later() {
        const D: HostId = HostId(5);
        let held_dead_after = |handshake: Option<u64>| {
            let (mut detector, now) = warmed();
            match handshake {
                Some(rtt) => detector
                    .join_measured(D, Duration::from_nanos(rtt))
                    .unwrap(),
                None => detector.join(D).unwrap(),
            }
            let mut state = 0x9E37_79B9_7F4A_7C15;
            let end = drive_many(
                &mut detector,
                now,
                now + 600_000 * MS,
                |peer| (peer != D).then(|| jitter(&mut state)),
                |_, _, _, _| false,
                |detector| liveness(detector, D) == Liveness::Dead,
            );
            assert_eq!(liveness(&detector, D), Liveness::Dead, "held dead");
            end - now
        };
        let unmeasured = held_dead_after(None);
        let measured = held_dead_after(Some(MS));
        eprintln!(
            "near peer held dead: {:.1} ms with no handshake round trip, {:.1} ms with one",
            unmeasured as f64 / MS as f64,
            measured as f64 / MS as f64
        );
        assert!(
            unmeasured <= measured + 6 * INITIAL_RTO_NS,
            "at most six initial RTOs later: {unmeasured} ns against {measured} ns"
        );
    }

    /// A far peer, joined with no handshake round trip, that answers its first probes in 40 ms and
    /// dies before its pair's estimator configures is condemned, by its pair's timer. A first form
    /// judged a pair the pool did not fit only once a round trip of its was measured, and a far
    /// pair whose answers all came after their records were reused had none: a member that then
    /// died was measured for ever (the hyper-raft owner's review of `swim-pair-deadline`, 2026-10-07).
    /// Do: on a configured member over near peers, join the far one, answer its first three probes
    /// in 40 ms, then nothing. Expect: it is held dead, by this member's own probes.
    #[test]
    fn a_far_peer_that_dies_before_its_estimator_configures_is_condemned() {
        const F: HostId = HostId(5);
        const FAR_RTT: u64 = 40 * MS;
        const ANSWERED: u32 = 3;
        let (mut detector, now) = warmed();
        detector.join(F).unwrap();
        let mut state = 0x9E37_79B9_7F4A_7C15;
        let mut answered = 0u32;
        drive_many(
            &mut detector,
            now,
            now + 600_000 * MS,
            |peer| {
                if peer != F {
                    return Some(jitter(&mut state));
                }
                answered += 1;
                (answered <= ANSWERED).then_some(FAR_RTT)
            },
            |_, _, _, _| false,
            |detector| liveness(detector, F) == Liveness::Dead,
        );
        let report = detector.report(F).unwrap();
        assert!(!report.configured, "its estimator never configured");
        assert_eq!(liveness(&detector, F), Liveness::Dead, "held dead");
        assert_eq!(report.condemnations, 1, "by this member's own probes");
    }

    #[test]
    fn an_isolated_member_condemns_nobody() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        world.run(&mut detector, 60, |_| None);
        for peer in peers {
            assert_eq!(liveness(&detector, peer), Liveness::Suspect, "{peer:?}");
            assert_eq!(detector.report(peer).unwrap().condemnations, 0);
        }
    }

    #[test]
    fn an_indirect_answer_spares_the_target() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        // Run until a probe of A is out, unanswered directly: one started in a run that leaves A
        // unanswered, not the last of the configuring runs, which answered everyone.
        let mut state = 3;
        loop {
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
            if world.pings.last().map(|p| p.to) == Some(A) {
                break;
            }
        }
        let ping = *world.pings.last().unwrap();
        world.now = ping_sent(&detector);
        assert_eq!(detector.poll(world.now, &mut world.requests), None);
        assert!(!world.requests.is_empty(), "relays asked at the deadline");
        assert!(
            world
                .requests
                .iter()
                .all(|r| r.target == A && r.nonce == ping.nonce)
        );
        let until = detector.wake().unwrap();
        assert!(until > world.now);
        detector.on_indirect_ack(A, ping.nonce, until - 1);
        world.now = until;
        assert!(detector.poll(world.now, &mut world.requests).is_some());
        assert_eq!(liveness(&detector, A), Liveness::Alive);
    }

    /// Runs `world` until a probe of `target` is out and its direct deadline has passed unanswered,
    /// every other peer answering, and polls there: the relays it asked are in `world.requests`.
    /// The probe.
    fn relays_asked_about(detector: &mut Detector, world: &mut World, target: HostId) -> Ping {
        let mut state = 3;
        loop {
            world.run(detector, 0, |peer| {
                (peer != target).then(|| jitter(&mut state))
            });
            if world.pings.last().map(|p| p.to) == Some(target) {
                break;
            }
        }
        let ping = *world.pings.last().unwrap();
        world.now = ping_sent(detector);
        assert_eq!(detector.poll(world.now, &mut world.requests), None);
        ping
    }

    /// A relay stage lasts until the slowest relay asked has its response due: this member's span
    /// for the relay, which bounds the request's way there and the response's way back, plus the
    /// bound the relay stated for responding. A peer that has stated none is not asked.
    #[test]
    fn a_relay_stage_waits_for_the_slowest_relay_s_stated_bound() {
        let peers = [A, B, C, HostId(5)];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 3;
        loop {
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
            if world.pings.last().map(|p| p.to) == Some(A) {
                break;
            }
        }
        let ping = *world.pings.last().unwrap();
        detector.peers.get_mut(&B).unwrap().relay_within_ns = Some(3 * MS);
        detector.peers.get_mut(&C).unwrap().relay_within_ns = Some(40 * MS);
        detector.peers.get_mut(&HostId(5)).unwrap().relay_within_ns = None;
        world.now = ping_sent(&detector);
        assert_eq!(detector.poll(world.now, &mut world.requests), None);
        let mut asked: Vec<HostId> = world.requests.iter().map(|r| r.relay).collect();
        asked.sort_unstable();
        assert_eq!(
            asked,
            vec![B, C],
            "the peer that stated nothing is not asked"
        );
        assert!(
            world
                .requests
                .iter()
                .all(|r| r.target == A && r.nonce == ping.nonce)
        );
        let due = |relay: HostId, within: u64| detector.verdict(relay).unwrap().span_ns() + within;
        let until = world.now + due(B, 3 * MS).max(due(C, 40 * MS));
        assert_eq!(detector.probe.unwrap().indirect_until, Some(until));
        assert_eq!(detector.wake(), Some(until));
        assert_eq!(detector.poll(until - 1, &mut world.requests), None);
        assert!(detector.poll(until, &mut world.requests).is_some());
        assert_eq!(liveness(&detector, A), Liveness::Suspect);
    }

    /// The relay report names the latest any relay this member could ask has its response due, and
    /// how many peers have stated a bound; nothing is due while none has.
    #[test]
    fn the_relay_report_is_the_slowest_relay_s_due() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        assert_eq!(detector.relays(), RelayReport::default());
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        detector.peers.get_mut(&B).unwrap().relay_within_ns = Some(3 * MS);
        detector.peers.get_mut(&C).unwrap().relay_within_ns = Some(40 * MS);
        detector.peers.get_mut(&A).unwrap().relay_within_ns = None;
        let due = |relay: HostId, within: u64| detector.verdict(relay).unwrap().span_ns() + within;
        let slowest = due(B, 3 * MS).max(due(C, 40 * MS));
        assert_eq!(
            detector.relays(),
            RelayReport {
                due: Some(Duration::from_nanos(slowest)),
                stated: 2,
            }
        );
    }

    /// A relay stage ends once every relay it asked has nacked, before the slowest relay's response
    /// is due: each judged its own leg to the target by its own deadline and had no answer
    /// (Lifeguard's nack, Dadgar et al. §IV-A). One relay's nack leaves the stage waiting on the
    /// other, and a nack about another probe moves nothing.
    #[test]
    fn a_relay_stage_ends_once_every_relay_asked_has_nacked() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let ping = relays_asked_about(&mut detector, &mut world, A);
        let asked: Vec<HostId> = world.requests.iter().map(|r| r.relay).collect();
        assert_eq!(asked.len(), 2, "both relays asked: {asked:?}");
        let until = detector.wake().unwrap();
        let at = world.now + 1;
        detector.on_indirect_nack(asked[0], A, ping.nonce, at);
        assert_eq!(detector.poll(at, &mut world.requests), None);
        assert_eq!(detector.wake(), Some(until), "one relay has not responded");
        detector.on_indirect_nack(asked[1], A, ping.nonce - 1, at);
        assert_eq!(detector.wake(), Some(until), "a nack about another probe");
        detector.on_indirect_nack(asked[1], A, ping.nonce, at + 1);
        assert!(detector.wake().unwrap() <= at + 1, "every relay has nacked");
        assert!(detector.poll(at + 1, &mut world.requests).is_some());
        assert_eq!(liveness(&detector, A), Liveness::Suspect);
        assert!(matches!(
            detector.findings(),
            [Finding::Suspected(Unanswered { target: A, relays_due_ns: Some(due), ended_ns, .. })]
                if *due == until && *ended_ns == at + 1
        ));
    }

    /// Takes from `pending` every answer due by `now`, earliest first.
    fn take_due(pending: &mut Vec<(HostId, u64, u64)>, now: u64) -> Vec<(HostId, u64, u64)> {
        pending.sort_by_key(|(_, _, at)| *at);
        let due = pending
            .iter()
            .copied()
            .filter(|(_, _, at)| *at <= now)
            .collect();
        pending.retain(|(_, _, at)| *at > now);
        due
    }

    /// A peer reachable only through relays whose answers come long after the direct deadline
    /// (slates, 2026-10-10: at load 100 a member that lost its direct path to B timed the relay
    /// stage from two direct spans, one frozen since B stopped answering it; relayed answers came for
    /// all 37 requests, but two came after their periods had ended, and each was a suspicion and
    /// then, its told probe missing too, a condemnation of a live member). Do: under a 900 ms budget,
    /// configure three pairs, then answer no direct probe of A and every relay request about A 150
    /// to 160 ms after it was asked; each relay states it responds within 160 ms, as its own
    /// deadline for A would, its probes of A measuring that slow path; every other probe answers in
    /// about 1 ms; A refutes each suspicion 500 ms after it starts; run 60 s. Expect: A is never
    /// suspected, so never condemned. When this member timed the stage by its own spans, every
    /// relayed answer about A came after it, and A was suspected and condemned 655 ms in.
    #[test]
    fn a_peer_answering_only_through_a_slow_relay_is_never_condemned() {
        const RELAYED_RTT: u64 = 150 * MS;
        const RELAYED_SPREAD: u64 = 10 * MS;
        const REFUTATION_DELAY: u64 = 500 * MS;
        const RUN: u64 = 60_000 * MS;
        let peers = [A, B, C];
        let mut detector = budgeted(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 0x2545_F491_4F6C_DD1D;
        let mut requests = Vec::new();
        let mut direct: Vec<(HostId, u64, u64)> = Vec::new();
        let mut relayed: Vec<(HostId, u64, u64)> = Vec::new();
        let mut now = world.now;
        let end = now + RUN;
        let mut refutation: Option<(u64, u64)> = None;
        while now < end && liveness(&detector, A) != Liveness::Dead {
            if let Some(ping) = detector.poll(now, &mut requests)
                && ping.to != A
            {
                direct.push((ping.to, ping.nonce, now + jitter(&mut state)));
            }
            if let Some(held) = detector.membership().state(A)
                && held.liveness == Liveness::Suspect
                && refutation.is_none()
            {
                refutation = Some((now + REFUTATION_DELAY, held.incarnation + 1));
            }
            if let Some((at, incarnation)) = refutation
                && at <= now
            {
                detector.apply(A, alive(incarnation)).unwrap();
                refutation = None;
            }
            for request in requests.drain(..) {
                let rtt = if request.target == A {
                    RELAYED_RTT + noise(&mut state) % RELAYED_SPREAD
                } else {
                    2 * jitter(&mut state)
                };
                relayed.push((request.target, request.nonce, now + rtt));
            }
            let next = direct
                .iter()
                .chain(relayed.iter())
                .map(|(_, _, at)| *at)
                .chain(refutation.map(|(at, _)| at))
                .min();
            now = match (detector.wake().map(|wake| wake + MS / 10), next) {
                (Some(wake), Some(at)) => wake.min(at).max(now),
                (Some(wake), None) => wake.max(now),
                (None, Some(at)) => at.max(now),
                (None, None) => {
                    detector.on_ping(B);
                    now + MS
                }
            };
            for (from, nonce, at) in take_due(&mut direct, now) {
                detector.on_ack(from, nonce, at, RELAYED_RTT + RELAYED_SPREAD);
            }
            for (target, nonce, at) in take_due(&mut relayed, now) {
                detector.on_indirect_ack(target, nonce, at);
            }
        }
        let report = detector.report(A).unwrap();
        assert_eq!(
            (report.suspicions, report.condemnations),
            (0, 0),
            "a peer answering through relays within their stated bound is never suspected ({} ms \
             in): {report:?}",
            (now - world.now) / MS
        );
    }

    /// An answer from a suspect to the probe that told it, however late, shows it alive after that
    /// probe: the condemnation pending on its miss is taken back, and the next answer from another
    /// member condemns nobody. Before, only a timely answer or a refutation took it back, and a
    /// suspect whose answers all came late was condemned at the next answer from anyone else.
    #[test]
    fn a_told_probe_s_late_answer_takes_back_its_pending_condemnation() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 5;
        world.found.clear();
        let told = loop {
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
            if let Some(Finding::Pending(missed)) = world
                .found
                .iter()
                .find(|finding| matches!(finding, Finding::Pending(_)))
            {
                break *missed;
            }
        };
        assert_eq!(liveness(&detector, A), Liveness::Suspect);
        assert!(detector.report(A).unwrap().pending_since_ns.is_some());
        let late = world.now;
        detector.on_ack(A, told.nonce, late, detector.relay_within_ns(A));
        assert_eq!(detector.report(A).unwrap().pending_since_ns, None);
        // The next periods' answers from the others condemn nobody.
        world.found.clear();
        let probes = world.pings.len();
        while world.pings[probes..].iter().all(|ping| ping.to == A)
            || world.pings.len() < probes + 2
        {
            world.run(&mut detector, 0, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
        }
        assert!(
            !world
                .found
                .iter()
                .any(|finding| matches!(finding, Finding::Condemned { target: A, .. })),
            "{:?}",
            world.found
        );
    }

    /// A relay's deadline for a ping it relays is its own for its probes of the target, within the
    /// longest it held for any member but the asker, which the bound it states to that asker covers;
    /// a target it does not hold is given that longest. The bound it states to a member leaves out
    /// its pair with that member, which it is never asked about.
    #[test]
    fn a_relay_states_and_keeps_its_bound_without_its_pair_with_the_asker() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let granularity = detector.granularity_ns();
        detector.refresh_extremes();
        detector.extremes.first = 50 * MS;
        detector.extremes.of = Some(A);
        detector.extremes.second = 20 * MS;
        detector.extremes.second_of = Some(B);
        assert_eq!(detector.on_ping(A).relay_within_ns, 20 * MS + granularity);
        assert_eq!(detector.on_ping(B).relay_within_ns, 50 * MS + granularity);
        let now = world.now;
        let unknown = HostId(99);
        assert_eq!(detector.on_ping_req(A, unknown, now).due_ns, now + 20 * MS);
        assert_eq!(detector.on_ping_req(B, unknown, now).due_ns, now + 50 * MS);
        let span = detector.verdict(C).unwrap().span_ns();
        assert!(span < 20 * MS);
        assert_eq!(
            detector.on_ping_req(A, C, now).due_ns,
            now + span,
            "the relay's own deadline for the target"
        );
    }

    #[test]
    fn a_refutation_clears_the_suspicion_and_its_pending_condemnation() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let mut state = 5;
        while liveness(&detector, A) != Liveness::Suspect {
            world.run(&mut detector, 1, |peer| {
                (peer != A).then(|| jitter(&mut state))
            });
        }
        detector
            .apply(
                A,
                MemberState {
                    liveness: Liveness::Alive,
                    incarnation: 1,
                },
            )
            .unwrap();
        world.run(&mut detector, 30, |_| Some(jitter(&mut state)));
        assert_eq!(liveness(&detector, A), Liveness::Alive);
        assert_eq!(detector.report(A).unwrap().condemnations, 0);
    }

    /// The probes of a suspect that tell it: its condemnation is pending once the first goes
    /// unanswered, and made at the next answer from another member; an extension buys exactly one
    /// more. Each finding carries its probe, and the condemnation the pending one's end.
    #[test]
    fn an_extension_buys_one_more_told_probe() {
        let peers = [A, B, C];
        // The told probe whose miss made the condemnation pending: the first after the suspicion,
        // or with an extension the second.
        let pending_after = |extend: bool| {
            let mut detector = detector(&peers);
            let mut world = World::new();
            configured(&mut detector, &mut world, &peers);
            let mut state = 9;
            while liveness(&detector, A) != Liveness::Suspect {
                world.run(&mut detector, 0, |peer| {
                    (peer != A).then(|| jitter(&mut state))
                });
            }
            if extend {
                assert_eq!(
                    detector.request_extension(A, 1, false),
                    ExtensionDecision::Granted { periods: 1 }
                );
                assert_eq!(
                    detector.request_extension(A, 2, false),
                    ExtensionDecision::Denied(ExtensionDenial::RateLimited)
                );
            }
            // A's probes are at most 2m − 1 periods apart (SWIM §4.3), and the answer from another
            // member that condemns comes at most two periods after the told probe's (A last in one
            // round and first in the next): the condemnation within that many periods a told
            // probe, and two.
            let told = if extend { 2 } else { 1 };
            let limit = (2 * peers.len() - 1) * told + 2;
            let suspected = world.pings.len();
            while liveness(&detector, A) != Liveness::Dead {
                world.run(&mut detector, 0, |peer| {
                    (peer != A).then(|| jitter(&mut state))
                });
                assert!(
                    world.pings.len() - suspected <= limit,
                    "not condemned within {limit} periods"
                );
            }
            let suspicion = world
                .found
                .iter()
                .rev()
                .find_map(|found| match found {
                    Finding::Suspected(missed) if missed.target == A => Some(*missed),
                    _ => None,
                })
                .unwrap();
            let told_probes: Vec<u64> = world
                .pings
                .iter()
                .filter(|ping| ping.to == A && ping.nonce > suspicion.nonce)
                .map(|ping| ping.nonce)
                .collect();
            let pending: Vec<Unanswered> = world
                .found
                .iter()
                .filter_map(|found| match found {
                    Finding::Pending(missed) if missed.target == A => Some(*missed),
                    _ => None,
                })
                .collect();
            assert_eq!(pending.len(), 1);
            let condemned: Vec<(u64, HostId)> = world
                .found
                .iter()
                .filter_map(|found| match found {
                    Finding::Condemned {
                        target,
                        pending_since_ns,
                        answered,
                        ..
                    } if *target == A => Some((*pending_since_ns, *answered)),
                    _ => None,
                })
                .collect();
            assert_eq!(condemned.len(), 1);
            assert_eq!(condemned[0].0, pending[0].ended_ns);
            assert_ne!(condemned[0].1, A, "another member's answer");
            told_probes
                .iter()
                .position(|nonce| *nonce == pending[0].nonce)
                .unwrap()
        };
        assert_eq!(pending_after(false), 0, "the first told probe");
        assert_eq!(pending_after(true), 1, "the second");
        let mut detector = detector(&peers);
        assert_eq!(
            detector.request_extension(A, 1, false),
            ExtensionDecision::Denied(ExtensionDenial::NotSuspected)
        );
    }

    #[test]
    fn the_allowance_is_the_sum_of_the_judged_probes_bounds() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let before = detector.report(A).unwrap();
        let in_force = |detector: &Detector| {
            detector
                .probe
                .filter(|probe| probe.target == A)
                .map(|probe| probe.verdict.mistake)
        };
        let mut pending = in_force(&detector);
        let (mut sum, mut state) = (before.suspicion_allowance, 13);
        for _ in 0..60 {
            // One new probe a call: the one before it is resolved by then, its bound added.
            world.run(&mut detector, 0, |_| Some(jitter(&mut state)));
            if let Some(mistake) = pending.take() {
                sum += mistake;
            }
            pending = in_force(&detector);
        }
        let after = detector.report(A).unwrap();
        assert!(sum > before.suspicion_allowance);
        assert_eq!(after.suspicion_allowance, sum);
        assert_eq!(after.suspicions, 0);
    }

    #[test]
    fn the_dissemination_budget_is_swims_bound() {
        assert_eq!(gossip_transmits(1), 1);
        assert_eq!(gossip_transmits(2), 1);
        let fraction = f64::from(1u32 << crate::fixed::FRACTION_BITS);
        for n in 3..2_000usize {
            let t = gossip_transmits(n);
            let nf = n as f64;
            // The least whole count past n·ln n/(n − 2), at the fixed-point logarithm every host
            // computes alike.
            let ln = crate::fixed::log2_fixed(n as u64) as f64 / fraction * std::f64::consts::LN_2;
            let rounds = nf * ln / (nf - 2.0);
            assert!(
                f64::from(t - 1) <= rounds && rounds < f64::from(t),
                "{n}: {t} against {rounds}"
            );
            // SWIM §4.1: n^{−((2−4/n)λ−2)} members uninfected in expectation, below one exactly
            // when the exponent is positive. At the true logarithm, t transmissions make it
            // positive and t − 1 do not.
            let exponent = |transmits: u32| (2.0 - 4.0 / nf) * f64::from(transmits) / nf.ln() - 2.0;
            assert!(
                exponent(t) > 0.0,
                "{n}: {t} transmits leave a member uninfected"
            );
            assert!(exponent(t - 1) <= 0.0, "{n}: {t} is not the least");
        }
        assert_eq!(gossip_transmits(4), 3);
        assert_eq!(gossip_transmits(256), 6);
    }

    #[test]
    fn the_relays_are_the_fewest_at_least_as_reliable_as_the_direct_probe() {
        for loss in [1e-6, 1e-3, 0.05, 0.3] {
            let k = relay_count(loss, 100);
            let through = 1.0 - (1.0 - loss) * (1.0 - loss);
            assert!(through.powi(k as i32) <= loss, "{loss}: {k}");
            assert!(k == 1 || through.powi(k as i32 - 1) > loss, "{loss}: {k}");
        }
        assert_eq!(relay_count(0.3, 1), 1);
        assert_eq!(relay_count(0.3, 0), 0);
    }

    /// A membership change is disseminated its budget of times, then dropped.
    #[test]
    fn a_change_is_gossiped_a_bounded_number_of_times() {
        let mut detector = detector(&[]);
        detector.join(A).unwrap();
        let mut batch = Vec::new();
        detector.gossip_into(10, &mut batch);
        assert!(batch.iter().any(|(host, _)| *host == A), "sent once");
        detector.gossip_into(10, &mut batch);
        assert!(
            batch.iter().all(|(host, _)| *host != A),
            "one member and us: one transmit"
        );
    }

    /// Lifeguard's buddy system: a ping to a suspected member carries the suspicion even after its
    /// transmit budget is spent; a ping to another member does not.
    #[test]
    fn a_ping_to_a_suspected_member_always_carries_the_suspicion() {
        let mut detector = detector(&[A, B]);
        let suspicion = MemberState {
            liveness: Liveness::Suspect,
            incarnation: 0,
        };
        detector.apply(A, suspicion).unwrap();
        let mut batch = Vec::new();
        for _ in 0..5 {
            detector.gossip_into(10, &mut batch);
        }
        detector.ping_gossip_into(A, 10, &mut batch);
        assert!(batch.contains(&(A, suspicion)), "the buddy system");
        detector.ping_gossip_into(B, 10, &mut batch);
        assert!(batch.iter().all(|(host, _)| *host != A));
        detector
            .apply(
                A,
                MemberState {
                    liveness: Liveness::Alive,
                    incarnation: 1,
                },
            )
            .unwrap();
        for _ in 0..5 {
            detector.gossip_into(10, &mut batch);
        }
        detector.ping_gossip_into(A, 10, &mut batch);
        assert!(
            batch.iter().all(|(host, _)| *host != A),
            "refuted: not injected"
        );
    }

    #[test]
    fn gossip_carries_a_change_to_another_node() {
        let mut source = detector(&[A]);
        source
            .apply(
                A,
                MemberState {
                    liveness: Liveness::Dead,
                    incarnation: 0,
                },
            )
            .unwrap();
        let mut batch = Vec::new();
        source.gossip_into(10, &mut batch);
        let mut other = Detector::new(B, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        other.join(A).unwrap();
        other.apply_gossip(batch);
        assert_eq!(liveness(&other, A), Liveness::Dead);
    }

    /// An exchange does not bring back a record a member has forgotten: a member still inside its
    /// window pushes the death, and the one past it holds nothing of the member to override. Before,
    /// it took the death as a newcomer's, restarting its window, and pushed it back in turn.
    #[test]
    fn an_exchange_does_not_bring_back_a_forgotten_record() {
        let dead = MemberState {
            liveness: Liveness::Dead,
            incarnation: 0,
        };
        let mut world = World::new();
        let mut first = running(LOCAL, &[A, C], &mut world);
        let mut second = running(A, &[LOCAL, C], &mut world);
        first.apply(C, dead).unwrap();
        second.apply(C, dead).unwrap();
        first.forget(C);
        assert_eq!(first.membership().state(C), None);
        assert_eq!(until_exchange(&mut first, &mut world), A);
        exchange_views(&mut first, &mut second, 4);
        assert_eq!(first.membership().state(C), None, "still forgotten");
    }

    #[test]
    fn a_dead_member_is_not_probed_while_another_lives() {
        let mut detector = detector(&[A, B]);
        detector
            .apply(
                A,
                MemberState {
                    liveness: Liveness::Dead,
                    incarnation: 0,
                },
            )
            .unwrap();
        let mut requests = Vec::new();
        for at in 0..6 {
            let ping = detector.poll(at, &mut requests).unwrap();
            assert_eq!(ping.to, B);
            detector.on_ack(B, ping.nonce, at, detector.relay_within_ns(B));
        }
    }

    /// Two members that hold each other dead: neither has anyone else to probe, so each probes the
    /// other and tells it, and each refutes in its answer. Without it neither ever sent again
    /// (the cluster test's deadlock under a one-CPU throttle).
    #[test]
    fn members_that_hold_each_other_dead_heal() {
        let (mut x, mut y) = (
            detector(&[A]),
            Detector::new(A, Exposure::new(), room(), RESOLUTION, Duration::ZERO),
        );
        y.join(LOCAL).unwrap();
        let dead = |host| {
            (
                host,
                MemberState {
                    liveness: Liveness::Dead,
                    incarnation: 0,
                },
            )
        };
        x.apply_gossip([dead(A)]);
        y.apply_gossip([dead(LOCAL)]);
        let mut detectors = [x, y];
        let mut requests = Vec::new();
        let mut batch = Vec::new();
        for at in 1..4u64 {
            for prober in 0..2 {
                let [first, second] = &mut detectors;
                let (prober, answering, from) = if prober == 0 {
                    (first, second, LOCAL)
                } else {
                    (second, first, A)
                };
                let Some(ping) = prober.poll(at * MS, &mut requests) else {
                    continue;
                };
                prober.ping_gossip_into(ping.to, 10, &mut batch);
                answering.apply_gossip(batch.iter().copied());
                answering.ack_gossip_into(from, 10, &mut batch);
                prober.apply_gossip(batch.iter().copied());
                prober.on_ack(
                    ping.to,
                    ping.nonce,
                    at * MS + 1,
                    prober.relay_within_ns(ping.to),
                );
            }
        }
        let [x, y] = detectors;
        assert_eq!(liveness(&x, A), Liveness::Alive);
        assert_eq!(liveness(&y, LOCAL), Liveness::Alive);
    }

    /// A refutation is a rumor, and a rumor can end known to some members and not all (Demers et
    /// al. 1987, §1.5): a member it missed holds the refuted member dead, past the record's window
    /// forgets it, and probes it no more. The refuted member's own messages carry its state, so
    /// the first it sends there revives it, held dead or forgotten. Without it the cluster test's
    /// second phase waited for ever, three runs in 1,119, on a member that had forgotten a live one.
    #[test]
    fn a_member_a_refutation_missed_hears_it_from_the_refuted_member() {
        let dead = MemberState {
            liveness: Liveness::Dead,
            incarnation: 0,
        };
        let mut refuted = detector(&[A, B]);
        refuted.apply(LOCAL, dead).unwrap();
        assert_eq!(refuted.membership().local_incarnation(), 1);
        // The refutation's rumor is spent before it reaches A or B.
        let mut batch = Vec::new();
        for _ in 0..8 {
            refuted.gossip_into(10, &mut batch);
        }
        assert!(batch.is_empty());
        let mut holds_dead = Detector::new(A, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        holds_dead.join(LOCAL).unwrap();
        holds_dead.apply(LOCAL, dead).unwrap();
        let mut forgot = Detector::new(B, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        forgot.join(LOCAL).unwrap();
        forgot.apply(LOCAL, dead).unwrap();
        forgot.forget(LOCAL);
        assert!(forgot.membership().state(LOCAL).is_none());
        let alive = (
            LOCAL,
            MemberState {
                liveness: Liveness::Alive,
                incarnation: 1,
            },
        );
        refuted.ping_gossip_into(A, 10, &mut batch);
        assert!(batch.contains(&alive));
        holds_dead.apply_gossip(batch.iter().copied());
        assert_eq!(liveness(&holds_dead, LOCAL), Liveness::Alive, "held dead");
        refuted.ping_gossip_into(B, 10, &mut batch);
        forgot.apply_gossip(batch.iter().copied());
        assert_eq!(liveness(&forgot, LOCAL), Liveness::Alive, "forgotten");
    }

    /// Carries every message of anti-entropy `from` has for `to`, as its owner would. Whether any.
    fn deliver(from: &mut Detector, to: &mut Detector, room: usize) -> bool {
        let mut batch = Vec::new();
        let mut any = false;
        while let Some(chunk) = from.sync_into(room, &mut batch) {
            assert_eq!(chunk.to, to.local);
            assert_eq!(chunk.digest, from.membership().digest());
            to.on_sync(from.local, chunk.digest, chunk.pull, batch.iter().copied());
            any = true;
        }
        any
    }

    /// Carries anti-entropy both ways between `first` and `second` until neither owes the other.
    fn exchange_views(first: &mut Detector, second: &mut Detector, room: usize) {
        for _ in 0..8 {
            let there = deliver(first, second, room);
            let back = deliver(second, first, room);
            if !there && !back {
                return;
            }
        }
        panic!("the exchange did not end");
    }

    /// A detector for `local` that knows `peers` and has run a few periods, everyone answering: it
    /// has a dissemination window.
    fn running(local: HostId, peers: &[HostId], world: &mut World) -> Detector {
        let mut detector =
            Detector::new(local, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        for &peer in peers {
            detector.join(peer).unwrap();
        }
        world.run(&mut detector, 4, |_| Some(MS));
        assert!(detector.dissemination_window_ns(world.now).is_some());
        // The exchange the run began, which the world does not carry.
        while detector.sync_into(64, &mut Vec::new()).is_some() {}
        detector
    }

    /// Runs `detector`'s periods, everyone it probes answering, until it begins an exchange: the
    /// partner its opening is for.
    fn until_exchange(detector: &mut Detector, world: &mut World) -> HostId {
        for _ in 0..1_000 {
            world.run(detector, 1, |_| Some(MS));
            if let Some(partner) = detector.exchanges.opening {
                return partner;
            }
        }
        panic!("no exchange began");
    }

    /// Two live members that each hold the other dead, both refutations missed, probe neither each
    /// other nor anyone about each other: a rumor can end known to some members and not all
    /// (Demers et al. 1987, §1.5), and the prober's own state reaches only the members it probes.
    /// An exchange with a third member that holds both alive revives each at the other: the
    /// digests differ, so the third answers with its view.
    #[test]
    fn a_mutual_split_heals_through_a_third_member() {
        let dead = MemberState {
            liveness: Liveness::Dead,
            incarnation: 0,
        };
        let mut world = World::new();
        let (x, y, z) = (LOCAL, A, B);
        let mut first = running(x, &[y, z], &mut world);
        let mut second = running(y, &[x, z], &mut world);
        let mut third = running(z, &[x, y], &mut world);
        // Each was condemned and refuted; the refutations reached the third member only.
        first.apply(x, dead).unwrap();
        second.apply(y, dead).unwrap();
        first.apply(y, dead).unwrap();
        second.apply(x, dead).unwrap();
        third.apply(x, alive(1)).unwrap();
        third.apply(y, alive(1)).unwrap();
        // The third member is the only one either holds alive, so the partner each exchange finds.
        assert_eq!(until_exchange(&mut first, &mut world), z);
        exchange_views(&mut first, &mut third, 4);
        assert_eq!(first.membership().state(y), Some(alive(1)));
        assert_eq!(until_exchange(&mut second, &mut world), z);
        exchange_views(&mut second, &mut third, 4);
        assert_eq!(second.membership().state(x), Some(alive(1)));
        assert_eq!(first.membership().digest(), third.membership().digest());
    }

    /// Two views that agree exchange their digests and nothing more: the opening earns no answer.
    #[test]
    fn agreeing_views_exchange_only_their_digests() {
        let mut world = World::new();
        let mut first = running(LOCAL, &[A], &mut world);
        let mut second = running(A, &[LOCAL], &mut world);
        assert_eq!(first.membership().digest(), second.membership().digest());
        assert_eq!(until_exchange(&mut first, &mut world), A);
        let mut batch = Vec::new();
        let opening = first.sync_into(64, &mut batch).unwrap();
        assert!(opening.pull && batch.is_empty(), "the digest alone");
        second.on_sync(LOCAL, opening.digest, opening.pull, batch.iter().copied());
        assert_eq!(second.sync_into(64, &mut batch), None, "nothing owed");
    }

    /// A view goes in chunks of the room, in id order, its own state included; only the first
    /// chunk of the answer to a differing opening asks a pull, and the push ends with the view.
    #[test]
    fn a_view_goes_in_chunks_of_the_room() {
        let peers: Vec<HostId> = (2..=10).map(HostId).collect();
        let mut world = World::new();
        let mut detector = running(LOCAL, &peers, &mut world);
        detector.on_sync(A, !detector.membership().digest(), true, []);
        let mut batch = Vec::new();
        let mut sent = Vec::new();
        let mut pulls = Vec::new();
        while let Some(chunk) = detector.sync_into(3, &mut batch) {
            assert_eq!(chunk.to, A);
            assert!(!batch.is_empty() && batch.len() <= 3);
            pulls.push(chunk.pull);
            sent.extend(batch.iter().map(|(host, _)| host.0));
        }
        assert_eq!(
            sent,
            (1..=10).collect::<Vec<_>>(),
            "the whole view, in id order"
        );
        assert_eq!(pulls, [true, false, false, false]);
    }

    /// A member owes one push at a time: a pull from another while it owes one is refused and
    /// counted, and answered once the first push is sent. A push that asks a pull earns one that
    /// asks nothing.
    #[test]
    fn a_member_owes_one_answer_at_a_time() {
        let mut detector = detector(&[A, B]);
        let other = !detector.membership().digest();
        detector.on_sync(A, other, true, []);
        detector.on_sync(A, other, true, []);
        detector.on_sync(B, other, true, []);
        assert_eq!(detector.pulls_refused(), 1);
        let mut batch = Vec::new();
        let first = detector.sync_into(10, &mut batch).unwrap();
        assert_eq!((first.to, first.pull), (A, true));
        assert_eq!(batch.len(), 3, "the whole view in one chunk");
        assert_eq!(detector.sync_into(10, &mut batch), None);
        detector.on_sync(B, other, true, [(B, alive(0))]);
        let answer = detector.sync_into(10, &mut batch).unwrap();
        assert_eq!((answer.to, answer.pull), (B, false));
    }

    /// An exchange begins once a dissemination window after the last began, and over a cycle of
    /// exchanges every member held alive is a partner once.
    #[test]
    fn an_exchange_begins_once_a_dissemination_window_and_cycles_its_partners() {
        let peers = [A, B, C];
        let mut world = World::new();
        let mut detector = running(LOCAL, &peers, &mut world);
        // A cycle starts afresh: the run began one, with its first partner.
        detector.exchanges.partners.clear();
        detector.exchanges.cursor = 0;
        let mut requests = Vec::new();
        let mut batch = Vec::new();
        let mut partners = Vec::new();
        for _ in 0..peers.len() {
            let partner = until_exchange(&mut detector, &mut world);
            let began = detector.exchanges.began_ns.unwrap();
            let window = detector.dissemination_window_ns(world.now).unwrap();
            partners.push(partner);
            let opening = detector.sync_into(10, &mut batch).unwrap();
            assert_eq!((opening.to, opening.pull), (partner, true));
            detector.poll(world.now, &mut requests);
            assert!(
                detector.exchanges.opening.is_none() || world.now - began >= window,
                "not again inside the window"
            );
            while detector.sync_into(10, &mut batch).is_some() {}
        }
        partners.sort();
        assert_eq!(partners, peers, "each partner once in a cycle");
    }

    /// A member held dead at its own incarnation, which it never heard, is told so by the answer to
    /// its probe (the buddy system, on answers as on probes, since a member does not probe the
    /// dead): it refutes, and its next message revives it.
    #[test]
    fn an_answer_tells_a_member_it_is_held_dead_and_its_next_probe_revives_it() {
        let dead = MemberState {
            liveness: Liveness::Dead,
            incarnation: 0,
        };
        let mut prober = detector(&[A]);
        let mut holder = Detector::new(A, Exposure::new(), room(), RESOLUTION, Duration::ZERO);
        holder.join(LOCAL).unwrap();
        holder.apply(LOCAL, dead).unwrap();
        let mut batch = Vec::new();
        for _ in 0..8 {
            holder.gossip_into(10, &mut batch);
        }
        assert!(batch.is_empty());
        // The probe states the prober alive at the incarnation it died at: the death stands.
        prober.ping_gossip_into(A, 10, &mut batch);
        holder.apply_gossip(batch.iter().copied());
        assert_eq!(liveness(&holder, LOCAL), Liveness::Dead);
        // The answer tells it so; it refutes.
        holder.ack_gossip_into(LOCAL, 10, &mut batch);
        assert!(batch.contains(&(LOCAL, dead)));
        prober.apply_gossip(batch.iter().copied());
        assert_eq!(prober.membership().local_incarnation(), 1);
        prober.ping_gossip_into(A, 10, &mut batch);
        holder.apply_gossip(batch.iter().copied());
        assert_eq!(liveness(&holder, LOCAL), Liveness::Alive);
    }

    #[test]
    fn every_peer_is_probed_once_per_round() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        let mut state = 1;
        for _ in 0..3 {
            let before = world.pings.len();
            world.run(&mut detector, peers.len() - 1, |_| Some(jitter(&mut state)));
            let mut round: Vec<u64> = world.pings[before..].iter().map(|p| p.to.0).collect();
            round.sort_unstable();
            assert_eq!(round, vec![2, 3, 4]);
        }
    }

    /// Every round trip measured to a peer whose coordinate is held teaches this member's
    /// coordinate once, by the engine's update with the peer's coordinate and the round's size:
    /// the detector's coordinate is, bit for bit, an engine's fed the same round trips.
    #[test]
    fn measured_round_trips_teach_the_coordinate() {
        let mut detector = detector(&[A]);
        let mut peer = NetworkCoordinate::origin();
        peer.position[0] = 0.030;
        peer.error = 0.05;
        detector.learn_coordinate(A, Coordinate::Held(&peer));
        assert!(detector.predicted_rtt(A).is_some());
        assert_eq!(detector.predicted_rtt(B), None);
        let mut engine = CoordinateEngine::new(LOCAL.0);
        let rtt = 35 * MS;
        let mut requests = Vec::new();
        let mut now = MS;
        for _ in 0..400 {
            let ping = loop {
                if let Some(ping) = detector.poll(now, &mut requests) {
                    break ping;
                }
                now = detector.wake().unwrap();
            };
            now += rtt;
            detector.on_ack(ping.to, ping.nonce, now, detector.relay_within_ns(ping.to));
            // A round of one: A is the only member probed.
            assert!(engine.update(&peer, Duration::from_nanos(rtt), 1));
            assert_eq!(detector.coordinate(), engine.coordinate());
        }
        assert_eq!(detector.predicted_rtt(A), Some(engine.predict(&peer)));
    }

    /// Each of `hosts` has acknowledged a probe, stating it responds to a ping-request within a
    /// millisecond.
    fn stated(detector: &mut Detector, hosts: &[HostId]) {
        for host in hosts {
            detector.peers.get_mut(host).unwrap().relay_within_ns = Some(MS);
        }
    }

    /// Relays are ranked nearest the target first, among the alive peers that have stated when
    /// their responses are due; one that has stated nothing is not asked, for nothing bounds the
    /// wait for its response.
    #[test]
    fn relays_are_ranked_nearest_the_target_first() {
        let positions = [(A, 0.0), (B, 1.0), (C, 2.0), (HostId(5), 10.0)];
        let mut detector = detector(&[]);
        for &(host, x) in &positions {
            detector.join(host).unwrap();
            let mut coordinate = NetworkCoordinate::origin();
            coordinate.position[0] = x;
            coordinate.error = 0.05;
            detector.learn_coordinate(host, Coordinate::Held(&coordinate));
        }
        detector.rank_relays(A);
        assert!(detector.relays.is_empty(), "no relay has stated a bound");
        stated(&mut detector, &[A, B, HostId(5)]);
        detector.rank_relays(A);
        assert_eq!(detector.relays, vec![B, HostId(5)]);
        stated(&mut detector, &[C]);
        detector.rank_relays(A);
        assert_eq!(detector.relays, vec![B, C, HostId(5)]);
    }

    /// A coordinate that cannot be used is not learned, and the one held stays.
    #[test]
    fn an_unusable_coordinate_is_not_learned() {
        let mut detector = detector(&[A]);
        let mut held = NetworkCoordinate::origin();
        held.position[0] = 0.002;
        detector.learn_coordinate(A, Coordinate::Held(&held));
        let mut broken = held;
        broken.height = f64::NAN;
        detector.learn_coordinate(A, Coordinate::Held(&broken));
        assert_eq!(detector.peer_coordinates.get(&A), Some(&held));
    }

    /// Relays are ranked by coordinates the engine learned from round trips: three regions of three
    /// members, a round trip the distance between two members' points and both their access heights,
    /// with a little jitter. Once every member has sampled the others, round after round, the relays
    /// a member ranks first for a target are the target's region-mates.
    #[test]
    fn relays_learned_from_round_trips_are_the_target_s_region() {
        // Points in milliseconds: regions 30 to 50 ms apart, members within a millisecond.
        let points: [(f64, f64); 9] = [
            (0.0, 0.0),
            (0.6, 0.2),
            (0.3, 0.7),
            (40.0, 0.0),
            (40.5, 0.4),
            (39.6, 0.6),
            (0.0, 30.0),
            (0.4, 30.5),
            (0.8, 29.8),
        ];
        let heights = [0.2, 0.3, 0.1, 0.25, 0.15, 0.35, 0.3, 0.2, 0.1];
        let rtt = |i: usize, j: usize| {
            let (a, b) = (points[i], points[j]);
            ((a.0 - b.0).hypot(a.1 - b.1) + heights[i] + heights[j]) / 1e3
        };
        let mut engines: Vec<CoordinateEngine> = (0..9u64).map(CoordinateEngine::new).collect();
        let mut state = 0x2545_F491_4F6C_DD1D;
        for _ in 0..300 {
            for i in 0..9 {
                for j in (0..9).filter(|j| *j != i) {
                    // Up to 2 % either way.
                    let jitter = 1.0 + ((noise(&mut state) % 41) as f64 - 20.0) / 1e3;
                    let peer = *engines[j].coordinate();
                    let sample = Duration::from_secs_f64(rtt(i, j) * jitter);
                    assert!(engines[i].update(&peer, sample, 8));
                }
            }
        }
        let hosts: Vec<HostId> = (0..9u64).map(|i| HostId(100 + i)).collect();
        let mut detector = Detector::new(
            hosts[0],
            Exposure::new(),
            room(),
            RESOLUTION,
            Duration::ZERO,
        );
        for (index, &host) in hosts.iter().enumerate().skip(1) {
            detector.join(host).unwrap();
            detector.learn_coordinate(host, Coordinate::Held(engines[index].coordinate()));
        }
        stated(&mut detector, &hosts[1..]);
        for target in 1..9 {
            detector.rank_relays(hosts[target]);
            let region = target / 3 * 3;
            let mut mates: Vec<HostId> = (region..region + 3)
                .filter(|member| *member != target && *member != 0)
                .map(|member| hosts[member])
                .collect();
            let mut first = detector.relays[..mates.len()].to_vec();
            mates.sort_unstable();
            first.sort_unstable();
            assert_eq!(first, mates, "target {target}: {:?}", detector.relays);
        }
    }
}
