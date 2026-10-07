//! The SWIM failure detector, its timing measured (`docs/timing.md` §2.7): the protocol-period
//! machine that probes members and drives the [`Membership`] view from alive to suspect to dead.
//! Sans-io: it is fed `now`, acknowledgements and pings, and returns the pings and ping-requests to
//! send and the time to be polled again ([`wake`](Detector::wake)).
//!
//! Evidence: SWIM (Das, Gupta and Motivala, DSN 2002) for the probe, the indirect probe, the
//! suspicion and infection-style dissemination; Lifeguard (Dadgar, Phillips and Currey, DSN 2018)
//! for the buddy system and for local health; Chen, Toueg and Aguilera (2002) for the detector each
//! probe stream is; `docs/research/timing.md` holds what each establishes.
//!
//! **Each pair is an NFD-E detector.** A member's probes to one peer and that peer's
//! acknowledgements are a heartbeat stream on the member's own clock: probe `k` sent at `s_k`,
//! answered at `A_k`, so NFD-E's delay `A_k − σ_k` is the probe's round trip, with no second clock
//! in it. A [`LinkEstimator`] per peer holds its mean, variance, loss, correlation and window, and
//! [`detector_at`] chooses the margin `α` that minimizes unavailability at the pair's probe
//! interval. A probe's acknowledgement is due at `s + μ + α`; if none came, the indirect probe asks
//! relays, and a probe answered by neither suspects the peer. The period is what its probe needs:
//! the direct deadline, and the indirect one when the direct passed unanswered (SWIM §3.1: the
//! protocol's properties hold for the average period).
//!
//! **Before a pair can be judged.** A pair's estimator refuses until it has its evidence
//! ([`Refusal`]). Its probes are then judged by the member's pooled estimator, every round trip the
//! member measured to anyone (the hosts' stalls, which the traces found dominate, are in it); and
//! while that too refuses, nobody is judged: a probe is measurement only. Its period ends when it
//! is answered or at its expected arrival from the latest round trip (NFD-E's estimate over a
//! window of one), whichever is first; unanswered then, it is a loss to the estimators unless its
//! answer comes later, and it judges nothing. Its wake measures the member's timer. Before any
//! round trip, the first probe waits on the round trip its owner measured to the peer, where it
//! gave one ([`Detector::join_measured`]), or as a retransmission timer does before its first
//! measurement ([`INITIAL_WAIT_NS`]), or until another member is heard from.
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
    Costs, Exposure, Floors, LinkBehaviour, LinkEstimator, Refusal, Schedule, Wakes, detector_at,
    mistake_bound,
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
    /// When its answer is due, on the caller's clock, as stated when it is sent: the probe's time
    /// plus its verdict's `μ + α`. `None` for a probe no verdict judges, whose period judges
    /// nothing, and for a probe relayed for another member, which that member times.
    pub due_ns: Option<u64>,
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
    /// When the relays' answers were due, where the direct deadline passed and relays were asked
    /// ([`PingReq`]).
    pub relays_due_ns: Option<u64>,
    /// When its period ended with no answer delivered, direct or relayed.
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

/// An acknowledgement to send to `to` — the reply to a received [`Ping`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ack {
    /// The member that pinged us.
    pub to: HostId,
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

/// What a member has done and promised about one peer, for its owner and its tests.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PeerReport {
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
}

/// Probes of one peer that may still be answered: the one that suspected it, the one that told it,
/// and the one more an extension can grant (at most one base window of one probe,
/// [`crate::extension`]). An older probe's acknowledgement is past every deadline that could use
/// it and is not kept for.
const OUTSTANDING: usize = 3;

/// How long a measurement period waits before any round trip was measured, nanoseconds: RFC 6298
/// §2.1, a retransmission timer's value until a round trip has been measured ("the sender SHOULD
/// set RTO <- 1 second"), backed off as any measurement period's wait is. Without it, a member
/// whose first probe or its answer was lost waited on another member, and members whose first
/// probes were all lost waited on one another for ever, none sending again.
const INITIAL_WAIT_NS: u64 = 1_000_000_000;

/// The longest a measurement period's wait backs off to, nanoseconds: RFC 6298 (2.5) lets a
/// retransmission timer's doubling be capped, provided the cap is at least 60 seconds.
const MEASUREMENT_WAIT_CAP_NS: u64 = 60_000_000_000;

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

    /// Configures the verdict from the estimates as they stand. A refusal leaves the verdict in
    /// force, as `LinkEstimator::configure` leaves its margin: a stall can make `τ_int` unmeasured
    /// again, and a probe that went unjudged then would suspect nobody.
    fn configure(&mut self, mtbf: Option<Duration>, floors: &Floors, interval: Duration) {
        let Some(estimator) = self.estimator.as_ref() else {
            return;
        };
        self.configured_at = self.samples;
        self.renewal = estimator.estimates().window.length;
        if let Ok(renewed) = verdict(estimator, mtbf, floors, interval) {
            self.verdict = Some(renewed);
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
    /// The previous judged probe's bound.
    last_mistake: Option<f64>,
    last_answer_ns: Option<u64>,
    /// Whether an answer showed the member's pooled verdict does not fit this pair: one came back
    /// past the pool's deadline, or after its probe's record had been reused by later probes. Such a
    /// pair is not judged by the pool: its probes are measurement only until its own estimator has a
    /// verdict, so its answers become its samples. A far peer among near ones was otherwise judged by
    /// a deadline drawn from the near round trips for ever: every answer arrived after its record was
    /// gone, so the pair never took a sample, never configured, and was condemned again and again
    /// while alive (slates, 2026-10-07: a 2 ms deadline on a 200 ms path, 76 to 115 condemnations a
    /// pair in 30 s).
    pool_misfit: bool,
    /// A pool-misfit pair's own measurement backoff: its unanswered measurement periods since its last
    /// sample. The member-wide backoff is reset by every other pair's answer, so a far pair among near
    /// ones would never wait long enough to be answered (RFC 6298 (5.5): the timer backs off per
    /// connection, doubling).
    misfit_misses: u32,
    report: PeerReport,
    /// A round trip to the peer its owner measured outside the detector, the handshake that keyed
    /// the peer's session ([`Detector::join_measured`]): the first wait for its probes before this
    /// member measured any round trip.
    handshake_rtt_ns: Option<u64>,
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
            last_mistake: None,
            last_answer_ns: None,
            pool_misfit: false,
            misfit_misses: 0,
            report: PeerReport::default(),
            handshake_rtt_ns: None,
        }
    }

    /// The outstanding probe `nonce`, taken.
    fn take(&mut self, nonce: u64) -> Option<Sent> {
        self.outstanding
            .iter_mut()
            .find(|slot| slot.is_some_and(|sent| sent.nonce == nonce))
            .and_then(Option::take)
    }

    /// Records a probe sent, over the oldest still outstanding.
    fn send(&mut self, sent: Sent) {
        let slots = u64::try_from(OUTSTANDING).unwrap_or(1);
        let at = usize::try_from(sent.seq.checked_rem(slots).unwrap_or(0)).unwrap_or(0);
        if let Some(slot) = self.outstanding.get_mut(at) {
            *slot = Some(sent);
        }
    }

    fn clear_pending(&mut self) {
        self.told_missed = 0;
        self.pending_since = None;
        self.report.pending_since_ns = None;
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
    verdict: Option<Verdict>,
    /// When the indirect probe's answers are due, once it was asked.
    indirect_until: Option<u64>,
    /// A measurement probe's expected arrival from the latest round trip, or before one from the
    /// owner's ([`Detector::wait_base`]), backed off while measurement periods go unanswered
    /// ([`Detector::measurement_wait`]): where its period ends, answered or not, judging nothing;
    /// its wake measures the member's timer.
    expected: u64,
}

impl Probe {
    fn due_ns(&self) -> Option<u64> {
        self.verdict
            .map(|verdict| self.sent_ns.saturating_add(verdict.span_ns()))
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
    membership: Membership,
    local: HostId,
    order: Vec<HostId>,
    cursor: usize,
    probe: Option<Probe>,
    /// Whether another member was heard from during a measurement probe.
    heard_other: bool,
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
    /// Round trips measured before this member had a wake and a period measured, which no
    /// estimator took.
    unmeasured: u64,
    /// The latest round trip measured, to anyone.
    last_rtt_ns: Option<u64>,
    /// Measurement periods ended unanswered since the latest round trip was measured: each doubles
    /// the next one's wait (RFC 6298 §5.5), up to [`MEASUREMENT_WAIT_CAP_NS`].
    measurement_misses: u32,
    /// The longest span `μ + α` any verdict of this member has had, nanoseconds.
    longest_span: u64,
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
    /// The deaths the view adopted, oldest first: the order their records are forgotten in. A
    /// record the member has since left (a refutation, a newer death) is skipped when reached.
    deaths: VecDeque<Death>,
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
    /// nanoseconds.
    pub fn new(
        local: HostId,
        history: Exposure,
        members: NonZeroUsize,
        resolution: Duration,
    ) -> Detector {
        Detector {
            membership: Membership::new(local, members),
            local,
            order: Vec::new(),
            cursor: 0,
            probe: None,
            heard_other: false,
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
            unmeasured: 0,
            last_rtt_ns: None,
            measurement_misses: 0,
            longest_span: 0,
            periods: Periods::default(),
            exposure: history,
            period: 0,
            extensions: BTreeMap::new(),
            aging: Vec::new(),
            relays: Vec::new(),
            deaths: VecDeque::new(),
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
        self.clock_ns = self.clock_ns.max(now_ns);
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
    /// indirect probe's once asked, or a measurement probe's expected arrival. `None` once a
    /// measurement probe is answered (poll on the next message), or before the first poll.
    pub fn wake(&self) -> Option<u64> {
        let probe = self.probe?;
        if probe.verdict.is_none() {
            return (!probe.answered).then_some(probe.expected);
        }
        if probe.answered {
            return probe.due_ns();
        }
        probe.indirect_until.or_else(|| probe.due_ns())
    }

    fn stage(&self, now_ns: u64) -> Stage {
        let Some(probe) = self.probe else {
            return Stage::Over;
        };
        let due = probe.due_ns();
        match (probe.answered, due, probe.indirect_until) {
            // Measurement: over when answered or at its expected arrival; before any round trip,
            // also when another member is heard from.
            (true, None, _) => Stage::Over,
            (false, None, _)
                if now_ns < probe.expected && !(self.last_rtt_ns.is_none() && self.heard_other) =>
            {
                Stage::Wait
            }
            (false, None, _) => Stage::Over,
            (true, Some(due), _) | (false, Some(due), None) if now_ns < due => Stage::Wait,
            (true, Some(_), _) => Stage::Over,
            (false, Some(_), None) => Stage::Indirect,
            (false, Some(_), Some(until)) if now_ns < until => Stage::Wait,
            (false, Some(_), Some(_)) => Stage::Over,
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
        let pooled_span = pooled.map(|verdict| verdict.span_ns());
        // The handshake that keyed the pair already measured its path: a round trip longer than the
        // pool's deadline is the same evidence a late answer gives, known before the first probe.
        let (own, misfit) = self.peers.get(&target).map_or((None, false), |peer| {
            let handshake_misfit = peer
                .handshake_rtt_ns
                .zip(pooled_span)
                .is_some_and(|(rtt, span)| rtt > span);
            (peer.stream.verdict, peer.pool_misfit || handshake_misfit)
        });
        let verdict = match own {
            Some(own) => Some(own),
            // A pair the pool's verdict does not fit is measured, not judged, until it has its own.
            None if misfit => None,
            None => pooled,
        };
        let peer = self.peers.entry(target).or_insert_with(Peer::new);
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
            expected: now_ns.saturating_add(self.probe_wait(target)),
        };
        self.probe = Some(probe);
        self.heard_other = false;
        Some(Ping {
            to: target,
            nonce,
            due_ns: probe.due_ns(),
        })
    }

    /// Fills `requests` with the relays to ask for the period's probe, and sets when their answers
    /// are due: the slowest relay's own deadline span (the leg to it and back) plus the target's
    /// (the relay's leg to the target, whose stalls are the target's own). With no relay to ask,
    /// nothing is due, and the period ends at the direct deadline.
    fn request_indirect(&mut self, now_ns: u64, requests: &mut Vec<PingReq>) {
        let Some(mut probe) = self.probe else {
            return;
        };
        let Some(verdict) = probe.verdict else {
            return;
        };
        self.rank_relays(probe.target);
        let count = relay_count(verdict.loss, self.relays.len());
        let mut slowest = 0u64;
        for &relay in self.relays.iter().take(count) {
            requests.push(PingReq {
                relay,
                target: probe.target,
                nonce: probe.nonce,
            });
            let span = self
                .peers
                .get(&relay)
                .and_then(|peer| peer.stream.verdict)
                .or(self.pool.verdict)
                .map_or(verdict.span_ns(), |relayed| relayed.span_ns());
            slowest = slowest.max(span);
        }
        if requests.is_empty() {
            return;
        }
        let until = now_ns
            .saturating_add(slowest)
            .saturating_add(verdict.span_ns());
        probe.indirect_until = Some(until);
        self.probe = Some(probe);
    }

    /// Ranks the alive peers other than `target` as relays, nearest the target first.
    fn rank_relays(&mut self, target: HostId) {
        let mut relays = std::mem::take(&mut self.relays);
        relays.clear();
        relays.extend(
            self.membership
                .alive()
                .filter(|host| *host != self.local && *host != target),
        );
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
        if probe.verdict.is_none()
            && !probe.answered
            && self.measurement_wait(self.wait_base(probe.target)) < MEASUREMENT_WAIT_CAP_NS
        {
            self.measurement_misses = self.measurement_misses.saturating_add(1);
        }
        let length = now_ns.saturating_sub(probe.sent_ns);
        self.periods.add(length);
        let watched = u32::try_from(self.order.len()).unwrap_or(u32::MAX);
        self.exposure
            .on_exposure(Duration::from_nanos(length).saturating_mul(watched));
        let Some(peer) = self.peers.get_mut(&probe.target) else {
            return;
        };
        Self::account(peer, probe.verdict);
        if peer.pool_misfit && probe.verdict.is_none() && !probe.answered {
            peer.misfit_misses = peer.misfit_misses.saturating_add(1);
        }
        if probe.answered {
            peer.clear_pending();
            self.condemn_pending(probe.target, probe.nonce, now_ns);
        } else if probe.verdict.is_some() {
            self.missed(probe, now_ns);
        }
    }

    /// Theorem 7's allowance for a judged probe: its bound for a suspicion, and for a
    /// condemnation the bound on it and the previous probe both missing, the lesser of the two
    /// (Fréchet's bound). Not their product: a pair's probes are a few periods apart, inside the
    /// stalls' correlation time the traces measured (20 to 250 ms, `docs/timing.md` §2.6), and a
    /// short history's `τ_int` of one has not yet seen a stall.
    fn account(peer: &mut Peer, verdict: Option<Verdict>) {
        let Some(verdict) = verdict else {
            peer.last_mistake = None;
            return;
        };
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
        let (Some(state), Some(due_ns)) = (self.membership.state(probe.target), probe.due_ns())
        else {
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
    /// span, then the slowest relay's and the target's again, at most three times the longest
    /// span any of its verdicts has had, plus the latest this member has woken past a wake it
    /// asked, or is late for now; the period in progress counts as run.
    ///
    /// `None` before a period, and while a probe this member makes would go unjudged, neither its
    /// pair nor the pool holding a verdict: an unjudged probe that goes unanswered suspects nobody,
    /// so until every probe is judged this member detects nothing by its own probes, and a death
    /// it holds then is another member's, adopted.
    pub fn detection_bound(&self, now_ns: u64) -> Option<Duration> {
        if !self.judges_every_probe() {
            return None;
        }
        // The most members the view has held besides this one: no round is larger, whatever the
        // rounds in the window were (a member that condemned another, even falsely, runs smaller
        // ones, and a member forgotten since was in rounds before).
        let spacing = self.most_watched.saturating_mul(2).saturating_sub(1);
        // The told probe's own period resolves it: one more.
        let periods = spacing.saturating_mul(2).saturating_add(1);
        let period = self.period_bound(now_ns)?;
        Some(Duration::from_nanos(period.saturating_mul(periods)))
    }

    /// How long a measurement period waits past its probe: the latest round trip, doubled for each
    /// measurement period since that ended unanswered, as a retransmission timer backs off (RFC 6298
    /// §5.5), up to [`MEASUREMENT_WAIT_CAP_NS`]. An answer is measured only while its probe is
    /// outstanding, the latest three of its peer's; when round trips lengthen past that, periods at
    /// the latest round trip's pace see every answer come for a probe written over, measure none,
    /// and never lengthen. A measured round trip ends the backing off.
    /// How long a measurement probe of `target` waits for its answer: the member's backed-off wait,
    /// or, for a pair the pool does not fit, that pair's own backoff from the same base.
    fn probe_wait(&self, target: HostId) -> u64 {
        let base = self.wait_base(target);
        let pooled_span = self.pool.verdict.map(|verdict| verdict.span_ns());
        match self.peers.get(&target) {
            Some(peer)
                if peer.pool_misfit
                    || peer
                        .handshake_rtt_ns
                        .zip(pooled_span)
                        .is_some_and(|(rtt, span)| rtt > span) =>
            {
                let factor = 1u64.checked_shl(peer.misfit_misses).unwrap_or(u64::MAX);
                base.saturating_mul(factor).min(MEASUREMENT_WAIT_CAP_NS)
            }
            _ => self.measurement_wait(base),
        }
    }

    fn measurement_wait(&self, rtt_ns: u64) -> u64 {
        let factor = 1u64
            .checked_shl(self.measurement_misses)
            .unwrap_or(u64::MAX);
        rtt_ns.saturating_mul(factor).min(MEASUREMENT_WAIT_CAP_NS)
    }

    /// The round trip a measurement probe of `target` waits from before backing off: the latest
    /// this member measured, to anyone; before any, the one its owner measured to `target`; and
    /// without one, [`INITIAL_WAIT_NS`].
    fn wait_base(&self, target: HostId) -> u64 {
        self.last_rtt_ns
            .or_else(|| {
                self.peers
                    .get(&target)
                    .and_then(|peer| peer.handshake_rtt_ns)
            })
            .unwrap_or(INITIAL_WAIT_NS)
    }

    /// Whether every probe this member makes is judged by a configured verdict: the pool holds
    /// one, or every member it probes has its own.
    fn judges_every_probe(&self) -> bool {
        self.pool.verdict.is_some()
            || self
                .peers
                .iter()
                .filter(|(host, _)| self.is_probed(**host))
                .all(|(_, peer)| peer.stream.verdict.is_some())
    }

    /// The longest a period of this member lasts, nanoseconds: the longest it has run or, where
    /// longer, what an unanswered probe's deadlines allow, three times the longest span any of its
    /// verdicts has had, plus how late it has woken; the period in progress counts as run. `None`
    /// before a period.
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
        let unanswered = self.longest_span.saturating_mul(3).saturating_add(late);
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

    /// Forgets `member`, held dead, and everything held of it.
    fn forget(&mut self, member: HostId) {
        if self.membership.forget(member) {
            self.peers.remove(&member);
            self.peer_coordinates.remove(&member);
            self.extensions.remove(&member);
            self.gossip.forget(member);
        }
    }

    /// Records an acknowledgement from `from` of the probe `nonce`, received at `at_ns` (the
    /// kernel's receive stamp where the caller has one, else when it was read). It answers the
    /// period's probe if it is that probe's, and its round trip is measured whatever probe it
    /// answers, however late: a late answer is the tail the margin must cover.
    pub fn on_ack(&mut self, from: HostId, nonce: u64, at_ns: u64) {
        match self.probe.as_mut() {
            Some(probe) if probe.target == from && probe.nonce == nonce => probe.answered = true,
            Some(probe) if probe.target != from => self.heard_other = true,
            _ => {}
        }
        self.clock_ns = self.clock_ns.max(at_ns);
        let measure = self.granularity().zip(self.periods.mean_ns());
        let interval = measure.map(|(_, period)| self.pair_interval(period));
        let mtbf = self.exposure.mtbf();
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        // Any answer is evidence of life when it arrives, even one too late to be measured.
        peer.last_answer_ns = Some(peer.last_answer_ns.map_or(at_ns, |last| last.max(at_ns)));
        let pooled_span = self.pool.verdict.map(|verdict| verdict.span_ns());
        let Some(sent) = peer.take(nonce) else {
            // An answer whose probe's record later probes already reused came back long after the
            // pool would have judged it: the pool does not fit this pair.
            if peer.stream.verdict.is_none() {
                peer.pool_misfit = true;
            }
            return;
        };
        let rtt = at_ns.saturating_sub(sent.at_ns);
        if peer.stream.verdict.is_none() && pooled_span.is_some_and(|span| rtt > span) {
            peer.pool_misfit = true;
        }
        peer.misfit_misses = 0;
        self.last_rtt_ns = Some(rtt);
        self.measurement_misses = 0;
        if let Some(((granularity, period), interval)) = measure.zip(interval) {
            peer.stream.take(sent.seq, rtt, granularity, interval);
            if peer.stream.due() {
                peer.stream.configure(mtbf, &floors(granularity), interval);
                peer.report.configured = peer.stream.verdict.is_some();
                if let Some(verdict) = peer.stream.verdict {
                    self.longest_span = self.longest_span.max(verdict.span_ns());
                }
            }
            // The pool is fed while it judges: by pairs with no verdict of their own, until it has
            // one.
            if !peer.report.configured || self.pool.verdict.is_none() {
                self.pool
                    .take(sent.nonce, rtt, granularity, Duration::from_nanos(period));
            }
        } else {
            self.unmeasured = self.unmeasured.saturating_add(1);
        }
        // The error estimate remembers one round: one probe of each member the round holds.
        if let Some(coordinate) = self.peer_coordinates.get(&from) {
            let round = self.order.len();
            self.coordinates
                .update(coordinate, Duration::from_nanos(rtt), round);
        }
    }

    /// The pool's verdict for a probe of a pair that has none of its own, renewed when due.
    fn pooled(&mut self) -> Option<Verdict> {
        if self.pool.due()
            && let Some((granularity, period)) = self.granularity().zip(self.periods.mean_ns())
        {
            let interval = self.pair_interval(period);
            self.pool
                .configure(self.exposure.mtbf(), &floors(granularity), interval);
            if let Some(verdict) = self.pool.verdict {
                self.longest_span = self.longest_span.max(verdict.span_ns());
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
    /// estimator took: with every pair's [`verdict`](Self::verdict) `None`, what tells an owner its
    /// member is measuring and not yet judging.
    pub fn unmeasured(&self) -> u64 {
        self.unmeasured
    }

    /// Records an indirect acknowledgement that `target` answered the probe `nonce` through a
    /// relay, at `at_ns`: the period's probe is answered. A relayed round trip is two paths' and
    /// is not the pair's sample.
    pub fn on_indirect_ack(&mut self, target: HostId, nonce: u64, at_ns: u64) {
        self.clock_ns = self.clock_ns.max(at_ns);
        if let Some(probe) = self.probe.as_mut()
            && probe.target == target
            && probe.nonce == nonce
        {
            probe.answered = true;
            if let Some(peer) = self.peers.get_mut(&target) {
                peer.last_answer_ns = Some(at_ns);
            }
        }
    }

    /// Responds to a ping from `from` with the acknowledgement to send back. A ping from a member
    /// other than the one being measured shows the network carries this member's traffic.
    pub fn on_ping(&mut self, from: HostId) -> Ack {
        if self.probe.is_some_and(|probe| probe.target != from) {
            self.heard_other = true;
        }
        Ack { to: from }
    }

    /// As a relay, the ping to send `target` for a ping-request; its nonce is the relay's own,
    /// from a range its own probes never use, and the caller maps the answer back to the asker.
    pub fn on_ping_req(&mut self, target: HostId) -> Ping {
        let nonce = self.relayed;
        self.relayed = self.relayed.saturating_sub(1);
        Ping {
            to: target,
            nonce,
            due_ns: None,
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
            last_answer_ns: held.last_answer_ns,
            ..held.report
        })
    }

    /// The round trips the pair's own estimator has taken of `peer`: the evidence it gathers
    /// toward its own configuration, whose longest window, `hyper_timing::WINDOW_LIMIT`, bounds
    /// what a configuration can need.
    pub fn round_trips_taken(&self, peer: HostId) -> Option<u64> {
        self.peers.get(&peer).map(|held| held.stream.samples)
    }

    /// The verdict that times this member's probes of `peer` now: the pair's, else the pool's.
    pub fn verdict(&self, peer: HostId) -> Option<Verdict> {
        self.peers
            .get(&peer)
            .and_then(|held| held.stream.verdict)
            .or(self.pool.verdict)
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
                self.exposure.on_failure();
                self.extensions.remove(&member);
                self.peer_coordinates.remove(&member);
                self.record_death(Death {
                    member,
                    incarnation: state.incarnation,
                    at_ns: None,
                });
            }
        }
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
    /// outside the detector, the handshake that keyed the peer's session: until this member
    /// measures a round trip, its probes of the peer wait on that one instead of
    /// [`INITIAL_WAIT_NS`], 1 s where a LAN's is about 100 µs. It judges nothing and feeds no
    /// estimator: a handshake is not a probe. A zero round trip measures nothing and is not taken.
    pub fn join_measured(&mut self, peer: HostId, round_trip: Duration) -> Result<(), Full> {
        self.join(peer)?;
        let rtt = u64::try_from(round_trip.as_nanos()).unwrap_or(u64::MAX);
        if let Some(held) = self.peers.get_mut(&peer)
            && rtt > 0
        {
            held.handshake_rtt_ns = Some(rtt);
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

/// The floors under a verdict: the member's granularity `G`, and the sender's `E[flush] + G = G`
/// (an acknowledgement is not flushed). The suspicion's margin holds one probe: SWIM judges each
/// probe on its own, and the condemnation that follows is the multi-probe rule
/// (`docs/timing.md` §2.7), so no correlation time enters.
fn floors(granularity: Duration) -> Floors {
    Floors {
        granularity,
        sender: granularity,
        correlation: Duration::MAX,
    }
}

/// The verdict for a probe stream: the margin minimizing unavailability at the pair's interval,
/// where a false suspicion costs the time until it is refuted — the pair's next probe, which
/// carries it, answered: `η + μ` — and a crash costs its detection.
fn verdict(
    estimator: &LinkEstimator,
    mtbf: Option<Duration>,
    floors: &Floors,
    interval: Duration,
) -> Result<Verdict, Refusal> {
    let link: LinkBehaviour = estimator.behaviour()?;
    let mtbf = mtbf.ok_or(Refusal::Unconfigurable)?;
    let costs = Costs {
        election: interval.saturating_add(link.mean_delay),
        mtbf,
    };
    let configured = detector_at(&link, &costs, floors, interval).ok_or(Refusal::Unconfigurable)?;
    let variance = link.delay_deviation.as_secs_f64().powi(2);
    let mistake = mistake_bound(
        link.loss,
        variance,
        interval.as_secs_f64(),
        configured.margin.as_secs_f64(),
    );
    Ok(Verdict {
        round_trip: link.mean_delay,
        margin: configured.margin,
        interval,
        loss: link.loss,
        mistake,
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
        let mut detector = Detector::new(LOCAL, Exposure::new(), room(), RESOLUTION);
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
                    detector.on_ack(from, nonce, at);
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
        detector.probe.unwrap().due_ns().unwrap()
    }

    fn liveness(detector: &Detector, peer: HostId) -> Liveness {
        detector.membership().state(peer).unwrap().liveness
    }

    /// A measurement probe whose ping or answer was lost ends at its expected arrival: with every
    /// member's probe lost at once (a throttled container dropping a burst), waiting for another
    /// member left all of them waiting for ever.
    #[test]
    fn a_lost_measurement_probe_ends_at_its_expected_arrival() {
        let mut detector = detector(&[A, B]);
        let mut requests = Vec::new();
        let first = detector.poll(0, &mut requests).unwrap();
        detector.on_ack(first.to, first.nonce, MS);
        let lost = detector.poll(MS, &mut requests).unwrap();
        assert_eq!(detector.verdict(lost.to), None, "measurement only");
        let expected = detector.wake().unwrap();
        assert_eq!(expected, 2 * MS, "the latest round trip on");
        assert_eq!(detector.poll(expected - 1, &mut requests), None);
        let next = detector.poll(expected, &mut requests);
        assert!(next.is_some(), "the period ended at the expected arrival");
        assert_eq!(
            liveness(&detector, lost.to),
            Liveness::Alive,
            "and judged nothing"
        );
    }

    /// A first probe, before any round trip, whose ping or answer was lost ends at the initial
    /// wait, and the next unanswered one at twice it: waiting on another member instead, members
    /// whose first probes were all lost waited on one another for ever (slates' daemons, the
    /// datagrams queued at a re-key dropped).
    #[test]
    fn a_lost_first_probe_ends_at_the_initial_wait() {
        let mut detector = detector(&[A, B]);
        let mut requests = Vec::new();
        let first = detector.poll(0, &mut requests).unwrap();
        assert_eq!(
            detector.wake(),
            Some(INITIAL_WAIT_NS),
            "a wake before any round trip"
        );
        assert_eq!(detector.poll(INITIAL_WAIT_NS - 1, &mut requests), None);
        let second = detector.poll(INITIAL_WAIT_NS, &mut requests).unwrap();
        assert_ne!(second.nonce, first.nonce, "probing again");
        assert_eq!(
            detector.wake(),
            Some(3 * INITIAL_WAIT_NS),
            "backed off: twice the initial wait"
        );
        assert_eq!(
            liveness(&detector, first.to),
            Liveness::Alive,
            "judging nothing"
        );
    }

    /// A peer joined with its handshake's round trip is first probed on that round trip's wait, not
    /// the initial one; a peer joined without one waits the initial wait, backed off.
    #[test]
    fn a_handshake_round_trip_sets_the_first_wait() {
        let mut detector = Detector::new(LOCAL, Exposure::new(), room(), RESOLUTION);
        let handshake = Duration::from_micros(100);
        detector.join_measured(A, handshake).unwrap();
        detector.join(B).unwrap();
        let mut requests = Vec::new();
        // Each period's target and its wait, the first unanswered one doubling the second's.
        let first = detector.poll(0, &mut requests).unwrap();
        let first_wait = detector.wake().unwrap();
        let second = detector.poll(first_wait, &mut requests).unwrap();
        let second_wait = detector.wake().unwrap() - first_wait;
        let base = u64::try_from(handshake.as_nanos()).unwrap();
        let expected = if first.to == A {
            [(A, base), (B, 2 * INITIAL_WAIT_NS)]
        } else {
            [(B, INITIAL_WAIT_NS), (A, 2 * base)]
        };
        assert_eq!([(first.to, first_wait), (second.to, second_wait)], expected);
    }

    /// Measurement periods follow round trips that lengthen. A measurement period ends at its
    /// expected arrival from the latest round trip; when the round trips lengthen past what the
    /// outstanding probes cover (three a peer), every answer comes for a probe written over, none
    /// is measured, the latest round trip never lengthens, and the member probes on at the stale
    /// pace and never configures: the cluster test's spin, a member 43,557 periods into a run with
    /// nothing judged while the others condemned it, slow to answer, 95 times.
    #[test]
    fn measurement_periods_follow_round_trips_that_lengthen() {
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
                    detector.on_ack(from, nonce, at);
                }
                !due
            });
        }
        assert!(
            detector.verdict(A).is_some() && detector.verdict(B).is_some(),
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
            &floors(g),
            Duration::from_millis(30),
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

    /// A member whose probes judge nothing yet states no detection bound: an unanswered probe
    /// suspects nobody until a verdict times it, so a death the member holds then is another's,
    /// adopted, on that member's timeline (the cluster test noted such a death, held from a
    /// gossiped condemnation 7 ms into the run, against a bound of 2.5 ms its probes could not
    /// keep).
    #[test]
    fn a_member_that_judges_nothing_states_no_bound() {
        let peers = [A, B];
        let mut detector = detector(&peers);
        let mut world = World::new();
        world.run(&mut detector, 2, |_| Some(MS));
        assert!(detector.periods.count > 0);
        assert!(peers.iter().all(|peer| detector.verdict(*peer).is_none()));
        assert_eq!(detector.detection_bound(world.now), None);
        configured(&mut detector, &mut world, &peers);
        assert!(detector.detection_bound(world.now).is_some());
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
        detector.on_ack(first.to, first.nonce, MS);
        assert_eq!(detector.unmeasured(), 1);
        assert_eq!(detector.round_trips_taken(first.to), Some(0));
    }

    /// The detection bound counts every member the view holds, not the current round's: a member
    /// that condemned two others (falsely, under a CPU throttle) ran rounds of one while the
    /// victim's last probe had been in a round of three, and stated a bound a third as long.
    #[test]
    fn the_detection_bound_does_not_shrink_with_the_round() {
        let peers = [A, B, C];
        let mut detector = detector(&peers);
        let mut world = World::new();
        configured(&mut detector, &mut world, &peers);
        let before = detector.detection_bound(world.now).unwrap();
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
        assert!(detector.detection_bound(world.now).unwrap() >= before);
        // Nor once the dead are forgotten: they were in the rounds before.
        while detector.membership().len() > 2 {
            world.run(&mut detector, 1, |_| Some(jitter(&mut state)));
        }
        assert!(detector.detection_bound(world.now).unwrap() >= before);
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

    #[test]
    fn nothing_is_judged_before_the_estimates_exist() {
        let mut detector = detector(&[A, B]);
        let mut world = World::new();
        world.heard = B;
        let mut state = 7;
        world.run(&mut detector, 40, |peer| {
            (peer == B).then(|| jitter(&mut state))
        });
        assert_eq!(detector.verdict(A), None, "no verdict from no evidence");
        assert_eq!(liveness(&detector, A), Liveness::Alive);
        assert_eq!(detector.report(A).unwrap().suspicions, 0);
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
            );
            detector.poll(world.now, &mut world.requests)
        });
        let ping = ping.unwrap();
        let verdict = detector.verdict(ping.to).unwrap();
        assert_eq!(detector.wake(), Some(world.now + verdict.span_ns()));
        assert!(verdict.round_trip >= Duration::from_millis(1));
        assert!(verdict.round_trip <= Duration::from_micros(1_500));
        assert!(verdict.mistake > 0.0 && verdict.mistake < 1.0);
        assert!(verdict.margin < verdict.interval, "one probe in the margin");
        // G is the lateness of the wakes: the world's, or less where an answer woke it first.
        let g = detector.granularity().unwrap();
        assert!(g > Duration::ZERO && g <= Duration::from_nanos(world.late));
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
                .and_then(|probe| probe.verdict)
                .map(|verdict| verdict.mistake)
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
        let mut other = Detector::new(B, Exposure::new(), room(), RESOLUTION);
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
            detector.on_ack(B, ping.nonce, at);
        }
    }

    /// Two members that hold each other dead: neither has anyone else to probe, so each probes the
    /// other and tells it, and each refutes in its answer. Without it neither ever sent again
    /// (the cluster test's deadlock under a one-CPU throttle).
    #[test]
    fn members_that_hold_each_other_dead_heal() {
        let (mut x, mut y) = (
            detector(&[A]),
            Detector::new(A, Exposure::new(), room(), RESOLUTION),
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
                prober.on_ack(ping.to, ping.nonce, at * MS + 1);
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
        let mut holds_dead = Detector::new(A, Exposure::new(), room(), RESOLUTION);
        holds_dead.join(LOCAL).unwrap();
        holds_dead.apply(LOCAL, dead).unwrap();
        let mut forgot = Detector::new(B, Exposure::new(), room(), RESOLUTION);
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
        let mut detector = Detector::new(local, Exposure::new(), room(), RESOLUTION);
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
        let mut holder = Detector::new(A, Exposure::new(), room(), RESOLUTION);
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
            detector.on_ack(ping.to, ping.nonce, now);
            // A round of one: A is the only member probed.
            assert!(engine.update(&peer, Duration::from_nanos(rtt), 1));
            assert_eq!(detector.coordinate(), engine.coordinate());
        }
        assert_eq!(detector.predicted_rtt(A), Some(engine.predict(&peer)));
    }

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
        let ranked: Vec<HostId> = detector.relays.clone();
        assert_eq!(ranked, vec![B, C, HostId(5)]);
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
        let mut detector = Detector::new(hosts[0], Exposure::new(), room(), RESOLUTION);
        for (index, &host) in hosts.iter().enumerate().skip(1) {
            detector.join(host).unwrap();
            detector.learn_coordinate(host, Coordinate::Held(engines[index].coordinate()));
        }
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
