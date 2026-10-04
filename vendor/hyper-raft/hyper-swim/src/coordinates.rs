//! Vivaldi network coordinates (§4.8 cluster plane): a decentralized synthetic coordinate system that
//! lets a node predict the round-trip time to a peer from coordinates it learns and round trips it
//! measures, without an all-pairs matrix. Dabek, Cox, Kaashoek and Morris, *Vivaldi: A
//! Decentralized Network Coordinate System*, SIGCOMM 2004; the sources and what each constant rests
//! on are in `docs/research/swim.md`.
//!
//! **The space** is Dabek's height vectors (§5.4): a point in the plane and a height, the time a packet
//! takes between the node and the core of the network, never negative. A difference is
//! `[x − y, x_h + y_h]` (the heights add), its length `‖x − y‖ + x_h + y_h`, so the predicted round
//! trip between two nodes is the distance between their points plus both heights. Two dimensions and a
//! height: the paper's principal components find two to three dimensions, extra dimensions past three
//! add nothing significant and cost communication (§5.2), and two with a height predict better than
//! two or three without (§5.4, Fig. 15).
//!
//! **The update** is Dabek's Fig. 3, whole. A node `i` that measured `rtt` to `j`:
//! - `w = e_i/(e_i + e_j)`, how much less sure of itself `i` is than `j`;
//! - `e_s = |‖x_i − x_j‖ − rtt| / rtt`, the sample's relative error;
//! - `e_i = e_s·c_e·w + e_i·(1 − c_e·w)`, the node's error a moving average of its samples';
//! - `δ = c_c·w`, and `x_i = x_i + δ·(rtt − ‖x_i − x_j‖)·u(x_i − x_j)`.
//!
//! The height moves inside that step, as the height vectors' algebra has it: the unit vector of
//! `[x_i − x_j, h_i + h_j]` divides the correction between the plane and the height by their shares of
//! the predicted round trip. `c_c` is the paper's (§4.1). `c_e`, which neither Dabek nor Ledlie,
//! Gardner and Seltzer give, is derived from what the estimate tracks: the node's error over its links,
//! which SWIM samples once each a round, so the estimate's memory is one round of `m` probes,
//! `c_e = 2/(m + 1)`, the weight whose moving average has the variance of an `m`-sample mean (NIST/
//! SEMATECH e-Handbook §6.3.2.4). There is no gravity: drift moves every coordinate alike and leaves
//! every prediction, and this engine predicts only between coordinates refreshed each round.
//!
//! **Determinism**: a coordinate is per-node state from that node's own measurements, so two nodes
//! hold different coordinates and nothing needs them bit-identical across hosts. Two fresh nodes,
//! both at the origin, meet the zero vector, whose direction Dabek draws at random (§2.4); the draw
//! comes from the node's own stream, seeded by the caller, so a simulation replays. Coordinates are an
//! operational estimate, never an identity, a hash or a tuning constant.

use std::f64::consts::TAU;
use std::time::Duration;

/// The plane's dimensions, besides the height: two (Dabek 2004 §5.2, §5.4 and Fig. 15).
pub const DIMENSIONS: usize = 2;
/// `c_c`, the largest fraction of a sample's correction a node moves by (Dabek 2004 §4.1, Fig. 5(b):
/// "Empirically, a `c_c` value of 0.25 yields both quick error reduction and low oscillation").
const TIMESTEP: f64 = 0.25;
/// One nanosecond, in seconds: the unit a `Duration` measures round trips in. A prediction error
/// below it cannot be measured, so a sample's error is at least it over the round trip, which keeps
/// the node's error estimate positive (an estimate of zero would fix `w` at zero, a node that never
/// moves again); and a height is kept at least it, so that it stays positive (§5.4).
const RESOLUTION: f64 = 1e-9;
/// The bits of a random word that make an `f64` in `[0, 1)`: its significand's 53.
const UNIT_BITS: u32 = 53;
/// The xorshift64 shift triple (Marsaglia, *Xorshift RNGs*, 2003), the detector's own generator's.
const XORSHIFT: [u32; 3] = [13, 7, 17];
/// The golden-ratio odd word (2^64 / φ) that mixes the seed, as the detector's probe order does, so
/// that nearby seeds draw visibly different directions.
const SEED_MIXER: u64 = 0x9E37_79B9_7F4A_7C15;

/// A node's network coordinate: its point in the plane, its height, and its estimate of its own
/// relative prediction error.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkCoordinate {
    /// The point in the plane, seconds.
    pub position: [f64; DIMENSIONS],
    /// The height, seconds: the time between the node and the core, never negative.
    pub height: f64,
    /// The node's estimate of its relative prediction error (Dabek §2.6); one is "no confidence".
    pub error: f64,
}

impl NetworkCoordinate {
    /// A node that has taken no sample: the origin, with no confidence (Dabek §2.4, §2.5).
    pub const fn origin() -> NetworkCoordinate {
        NetworkCoordinate {
            position: [0.0; DIMENSIONS],
            height: 0.0,
            error: 1.0,
        }
    }

    /// Whether the coordinate can be used: a finite point, a finite height not below zero, and an
    /// error not below zero (an infinite one is no confidence at all). A peer's coordinate arrives
    /// as its bits, and Vivaldi defends against peers in error, not against malicious ones (§6.2);
    /// one this refuses would otherwise make every coordinate it touches not a number.
    pub fn is_usable(&self) -> bool {
        self.position.iter().all(|component| component.is_finite())
            && self.height.is_finite()
            && self.height >= 0.0
            && self.error >= 0.0
    }
}

/// A node's Vivaldi coordinate engine: its own coordinate, moved by each round trip it measures.
pub struct CoordinateEngine {
    coordinate: NetworkCoordinate,
    /// The node's stream for the zero vector's direction: xorshift64's state, never zero.
    stream: u64,
}

impl CoordinateEngine {
    /// A fresh engine at the origin, drawing the zero vector's directions from a stream seeded by
    /// `seed` (the node's identity, so a simulation replays).
    pub fn new(seed: u64) -> CoordinateEngine {
        CoordinateEngine {
            coordinate: NetworkCoordinate::origin(),
            stream: (seed ^ SEED_MIXER) | 1,
        }
    }

    /// This node's coordinate.
    pub fn coordinate(&self) -> &NetworkCoordinate {
        &self.coordinate
    }

    /// The predicted round trip between two coordinates, seconds: the distance between their points
    /// plus both heights (§5.4). Symmetric, and never negative for usable coordinates.
    pub fn estimate_rtt(a: &NetworkCoordinate, b: &NetworkCoordinate) -> f64 {
        distance(&a.position, &b.position) + a.height + b.height
    }

    /// The predicted round trip from this node to `peer`, seconds.
    pub fn predict(&self, peer: &NetworkCoordinate) -> f64 {
        CoordinateEngine::estimate_rtt(&self.coordinate, peer)
    }

    /// Moves this node's coordinate by a round trip `rtt` measured to `peer`, Dabek's Fig. 3 whole
    /// (the module's documentation), with `c_e` the weight of one round of `round` probes. A round
    /// trip of zero, a peer coordinate that is not usable, or a sample that would leave this
    /// coordinate infinite or not a number (a peer's point so far out that the distance overflows)
    /// is no sample: whether this one was taken.
    pub fn update(&mut self, peer: &NetworkCoordinate, rtt: Duration, round: usize) -> bool {
        let rtt = rtt.as_secs_f64();
        if rtt <= 0.0 || !peer.is_usable() {
            return false;
        }
        let local = self.coordinate;
        let mut next = local;
        // Line 1. The node's own error is positive and finite (it starts at one, every sample's is
        // at least the resolution's, and a sample that is not finite is not taken), so the sum is
        // positive.
        let weight = local.error / (local.error + peer.error);
        // Line 2.
        let predicted = CoordinateEngine::estimate_rtt(&local, peer);
        let sample = (predicted - rtt).abs().max(RESOLUTION) / rtt;
        // Line 3.
        let moving = error_weight(round) * weight;
        next.error = sample * moving + local.error * (1.0 - moving);
        // Line 4: the step along the unit vector of the height-vector difference.
        let force = TIMESTEP * weight * (rtt - predicted);
        let (planar, height) = self.direction(&local, peer, predicted);
        for (component, unit) in next.position.iter_mut().zip(planar) {
            *component += force * unit;
        }
        next.height = (local.height + force * height).max(RESOLUTION);
        let finite = next.position.iter().all(|component| component.is_finite())
            && next.height.is_finite()
            && next.error.is_finite();
        if finite {
            self.coordinate = next;
        }
        finite
    }

    /// The unit vector of `local − peer` in the height-vector space, as its plane part and its
    /// height part: `[x_i − x_j, h_i + h_j] / (‖x_i − x_j‖ + h_i + h_j)`, whose length, `predicted`,
    /// is the predicted round trip. When it is zero (two nodes at one point, neither with a height)
    /// the direction is Dabek's `u(0)`, drawn at random: uniform on the space's unit sphere
    /// `‖v‖ + h = 1, h ≥ 0`, the height of density `2(1 − h)` and the angle uniform.
    fn direction(
        &mut self,
        local: &NetworkCoordinate,
        peer: &NetworkCoordinate,
        predicted: f64,
    ) -> ([f64; DIMENSIONS], f64) {
        if predicted > 0.0 {
            let mut planar = [0.0; DIMENSIONS];
            for ((unit, mine), theirs) in planar.iter_mut().zip(local.position).zip(peer.position) {
                *unit = (mine - theirs) / predicted;
            }
            return (planar, (local.height + peer.height) / predicted);
        }
        let (sin, cos) = (TAU * self.uniform()).sin_cos();
        let height = 1.0 - (1.0 - self.uniform()).sqrt();
        ([cos * (1.0 - height), sin * (1.0 - height)], height)
    }

    /// The next draw of the node's stream, uniform in `[0, 1)`.
    fn uniform(&mut self) -> f64 {
        let mut x = self.stream;
        x ^= x << XORSHIFT[0];
        x ^= x >> XORSHIFT[1];
        x ^= x << XORSHIFT[2];
        self.stream = x;
        let bits = x >> (u64::BITS - UNIT_BITS);
        // u64 → f64 is exact below 2⁵³.
        bits as f64 / (1u64 << UNIT_BITS) as f64
    }
}

/// `c_e` for a round of `round` probes: `2/(m + 1)`, the moving average whose variance is an
/// `m`-sample mean's, `s²·c/(2 − c) = s²/m` (NIST/SEMATECH e-Handbook §6.3.2.4): the error estimate
/// remembers one round, a sample of each of the node's links. A round of none or one is a round of one.
fn error_weight(round: usize) -> f64 {
    let probes = round.max(1) as f64;
    2.0 / (probes + 1.0)
}

/// The Euclidean distance between two points of the plane.
fn distance(a: &[f64; DIMENSIONS], b: &[f64; DIMENSIONS]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f64>()
        .sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: f64 = 1e-3;

    fn at(x: f64, y: f64, height: f64, error: f64) -> NetworkCoordinate {
        NetworkCoordinate {
            position: [x, y],
            height,
            error,
        }
    }

    /// Seconds in a millisecond's binary neighbourhood: 2⁻¹⁰ s, 0.98 ms. Every value below is a
    /// multiple of it with a short binary fraction, and a round trip of whole multiples converts
    /// from a `Duration` exactly, so each step of the update is exact and is checked bit for bit.
    const K: f64 = 1.0 / 1024.0;

    /// A round trip of `multiples` of [`K`], an even number: a whole count of nanoseconds, which
    /// converts back to seconds exactly.
    fn of(multiples: u32) -> Duration {
        let seconds = f64::from(multiples) * K;
        let rtt = Duration::from_secs_f64(seconds);
        assert_eq!(rtt.as_secs_f64(), seconds, "{multiples} converts exactly");
        rtt
    }

    /// One update is Dabek's Fig. 3 line by line, the height moving inside the spring (§5.4), and
    /// it moves the prediction `δ = c_c·w` of the way to the measured round trip.
    #[test]
    fn one_update_is_dabeks_figure_3() {
        let mut engine = CoordinateEngine::new(1);
        engine.coordinate = at(0.0, 0.0, 1.0 * K, 0.75);
        let peer = at(3.0 * K, 4.0 * K, 2.0 * K, 0.25);
        assert!(engine.update(&peer, of(16), 3));
        // ‖x_i − x_j‖ = 5, so the prediction is 5 + 1 + 2 = 8 against 16 measured;
        // w = 0.75/(0.75 + 0.25) = 0.75; e_s = |8 − 16|/16 = 0.5; c_e = 2/(3 + 1) = 0.5;
        // e_i = 0.5·0.5·0.75 + 0.75·(1 − 0.5·0.75) = 0.65625;
        // force = c_c·w·(16 − 8) = 0.25·0.75·8 = 1.5, along [−3, −4, 1 + 2]/8.
        assert_eq!(
            *engine.coordinate(),
            at(-0.5625 * K, -0.75 * K, 1.5625 * K, 0.65625)
        );
        assert_eq!(engine.predict(&peer), (8.0 + 1.5) * K);
    }

    /// The error estimate is a relative error, scale-free: the same round trips at a LAN's scale
    /// and at 2¹⁰ times it, a WAN's, leave the same estimate bit for bit at every sample, and
    /// heights 2¹⁰ times apart. The engine before folded the error in seconds and floored it at
    /// 0.05, 50 ms: both scales read 0.05.
    #[test]
    fn the_error_estimate_is_a_relative_error() {
        let step = |engine: &mut CoordinateEngine, scale: u64, sample: u64| {
            // 100 µs at the LAN's scale, 10 % either side by turns.
            let base = if sample.is_multiple_of(2) {
                110_000
            } else {
                90_000
            };
            let rtt_ns = base * scale;
            let peer = at(0.0, 0.0, 50e-6 * scale as f64, 0.0);
            assert!(engine.update(&peer, Duration::from_nanos(rtt_ns), 15));
            *engine.coordinate()
        };
        let (mut lan, mut wan) = (CoordinateEngine::new(7), CoordinateEngine::new(7));
        for sample in 0..4_000 {
            let (near, far) = (step(&mut lan, 1, sample), step(&mut wan, 1_024, sample));
            assert_eq!(near.error, far.error, "sample {sample}");
            assert_eq!(near.height * 1_024.0, far.height, "sample {sample}");
            assert_eq!((near.position, far.position), ([0.0; 2], [0.0; 2]));
        }
        // From a peer with no error, at a round of one, the estimate is the sample's error.
        let mut engine = CoordinateEngine::new(7);
        let peer = at(0.0, 0.0, 2.0 * K, 0.0);
        let predicted = engine.predict(&peer);
        assert!(engine.update(&peer, of(8), 1));
        assert_eq!(engine.coordinate().error, (8.0 * K - predicted) / (8.0 * K));
    }

    /// Fed a fixed peer's round trip, a coordinate closes the same share of the gap at every
    /// sample: from a peer with no error `w = 1`, so `δ = c_c`, a quarter, and the gap after `k`
    /// samples is `(3/4)^k` of the first. Exact while the gap's binary fraction fits a double's.
    #[test]
    fn a_coordinate_converges_to_predict_a_peer() {
        let peer = at(0.0, 0.0, 8.0 * K, 0.0);
        let rtt = of(16);
        let mut engine = CoordinateEngine::new(3);
        let mut gap = 8.0 * K;
        assert_eq!(engine.predict(&peer), 16.0 * K - gap);
        for sample in 1..=25 {
            assert!(engine.update(&peer, rtt, 1));
            gap *= 0.75;
            assert_eq!(engine.predict(&peer), 16.0 * K - gap, "sample {sample}");
            // At a round of one the estimate is the sample's relative error, the gap before it.
            assert_eq!(engine.coordinate().error, gap / 0.75 / (16.0 * K));
        }
    }

    /// Two fresh nodes at the origin meet the zero vector, and each steps along its own draw of
    /// Dabek's `u(0)` (§2.4) from its stream: an angle uniform in the plane and a height of density
    /// `2(1 − h)`. The step is `δ·rtt` along it, `δ = c_c·w = 0.125` with both errors one, the
    /// height floored at the resolution; bit for bit, and replayed from the seed.
    #[test]
    fn two_fresh_nodes_separate_and_replay() {
        let rtt = Duration::from_millis(4);
        let first = |seed| {
            let mut engine = CoordinateEngine::new(seed);
            engine.update(&NetworkCoordinate::origin(), rtt, 1);
            *engine.coordinate()
        };
        let (a, b) = (first(1), first(2));
        assert_ne!(a.position, b.position, "different directions");
        assert_eq!(first(1), a, "replayed");
        for (seed, coordinate) in [(1, a), (2, b)] {
            // The node's stream, drawn in the engine's order: the angle, then the height.
            let mut stream = CoordinateEngine::new(seed);
            let (sin, cos) = (TAU * stream.uniform()).sin_cos();
            let height = 1.0 - (1.0 - stream.uniform()).sqrt();
            let force = 0.125 * rtt.as_secs_f64();
            assert_eq!(
                coordinate.position,
                [
                    force * (cos * (1.0 - height)),
                    force * (sin * (1.0 - height))
                ]
            );
            assert_eq!(coordinate.height, (force * height).max(RESOLUTION));
            assert!(coordinate.height > 0.0, "a height of its own");
        }
    }

    /// A sample that is no sample leaves the coordinate as it was: a round trip of zero, or a peer
    /// coordinate that is not a number, has a negative height, or a negative error.
    #[test]
    fn an_unusable_sample_is_not_taken() {
        let mut engine = CoordinateEngine::new(5);
        let before = *engine.coordinate();
        let peer = at(1.0 * MS, 0.0, 0.0, 0.1);
        assert!(!engine.update(&peer, Duration::ZERO, 1));
        for broken in [
            at(f64::NAN, 0.0, 0.0, 0.1),
            at(0.0, f64::INFINITY, 0.0, 0.1),
            at(0.0, 0.0, -1.0, 0.1),
            at(0.0, 0.0, f64::NAN, 0.1),
            at(0.0, 0.0, 0.0, -0.5),
            at(0.0, 0.0, 0.0, f64::NAN),
        ] {
            assert!(!engine.update(&broken, Duration::from_millis(1), 1));
        }
        assert_eq!(engine.coordinate(), &before);
        // A peer with no confidence at all moves nothing, and is taken.
        assert!(engine.update(
            &at(0.0, 0.0, 0.0, f64::INFINITY),
            Duration::from_millis(1),
            1
        ));
        assert_eq!(engine.coordinate().position, before.position);
        // A peer whose point or height is finite but so far out that the distance or the sample's
        // error overflows would leave this coordinate not a number: not taken.
        let taken = *engine.coordinate();
        for far in [at(1e300, 1e300, 0.0, 0.1), at(0.0, 0.0, 1e308, 0.1)] {
            assert!(!engine.update(&far, Duration::from_nanos(1), 1));
        }
        assert_eq!(engine.coordinate(), &taken);
    }

    /// The estimate is symmetric and never negative.
    #[test]
    fn the_estimate_is_symmetric_and_nonnegative() {
        let a = at(3.0, -4.0, 1.0, 1.0);
        let b = at(-1.0, 0.0, 2.0, 1.0);
        let forward = CoordinateEngine::estimate_rtt(&a, &b);
        let backward = CoordinateEngine::estimate_rtt(&b, &a);
        assert_eq!(forward, backward, "symmetric");
        assert!(forward >= 0.0, "never negative");
    }

    /// `c_e` remembers one round: an EWMA of its weight has an `m`-sample mean's variance. With
    /// `m + 1` a power of two every operation is exact, and so is the equality.
    #[test]
    fn the_error_weight_remembers_one_round() {
        for m in [1usize, 3, 15, 255] {
            let c = error_weight(m);
            let variance = c / (2.0 - c);
            assert_eq!(variance, 1.0 / m as f64, "{m}");
        }
        assert_eq!(error_weight(0), error_weight(1));
    }
}
