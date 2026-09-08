//! Vivaldi network coordinates (§4.8 cluster plane) — a decentralized synthetic coordinate system that
//! lets a node **predict the round-trip time to any peer** from coordinates it learns from its own RTT
//! samples, without an all-pairs measurement matrix. Each node holds a point in a low-dimensional
//! Euclidean space plus a non-negative *height* (the last-hop / access-link latency every path shares)
//! and a small *adjustment* (a systematic local offset); the predicted RTT between two nodes is the
//! coordinate distance plus both heights and adjustments. A node relaxes its own coordinate toward each
//! measured RTT like a spring, weighted by how much more confident the peer's coordinate is than its own.
//!
//! Where it fits: SWIM already times out from measured RTT p99 (§4.8) and probes in randomized order;
//! coordinates *add* a per-peer RTT estimate the caller can use for a per-peer probe deadline (a distant
//! peer is given proportionally longer) and for proximity-aware indirect-probe relay selection, so a
//! slow far peer is not mistaken for a failed near one. This module is the pure coordinate engine; how a
//! detector consumes the estimate is the caller's (owed integration).
//!
//! Determinism: a coordinate is **per-node local state** derived from that node's own measurements —
//! two nodes legitimately hold different coordinates, so there is no cross-host bit-identity requirement
//! the way a seal or an ops document has. The deterministic simulation feeds RTT samples from its seed,
//! so a coordinate evolves reproducibly within a run. This is the one place the cluster plane uses
//! floating point, and only for an operational estimate — never for an identity, a hash or a tuning
//! constant (the Vivaldi constants below carry their own derivations).
//!
//! Evidence: Dabek, Cox, Kaashoek, Morris, *Vivaldi: A Decentralized Network Coordinate System*,
//! SIGCOMM 2004 (tier A); the height and adjustment extensions and the constant choices follow
//! hyperscale's `swim/coordinates/coordinate_engine.py` (tier C, deployed).

/// A node's network coordinate: the Euclidean position, the shared last-hop height, a systematic local
/// adjustment, and the node's confidence in its own position (a relative error in `[min_error,
/// max_error]`, one meaning "no confidence").
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkCoordinate {
  /// The position in the synthetic Euclidean space.
  pub vec: Vec<f64>,
  /// The non-negative height (last-hop latency added to every path from this node).
  pub height: f64,
  /// A small systematic offset applied to predictions involving this node.
  pub adjustment: f64,
  /// The node's confidence in its own coordinate — a relative prediction error; one is "no confidence".
  pub error: f64,
}

impl NetworkCoordinate {
  /// A fresh coordinate at the origin with full uncertainty (`error = 1`), for a node that has taken no
  /// samples yet.
  pub fn origin(dimensions: usize) -> NetworkCoordinate {
    NetworkCoordinate {
      vec: vec![0.0; dimensions],
      height: 0.0,
      adjustment: 0.0,
      error: 1.0,
    }
  }
}

/// Derived: the dimension of the coordinate space. Vivaldi finds 2–3 Euclidean dimensions plus a height
/// captures Internet RTT well (Dabek 2004 §4); more dimensions add accuracy with diminishing returns and
/// cost. Eight matches hyperscale's deployed choice, leaving margin for a large multi-region fleet.
const DIMENSIONS: usize = 8;
/// Derived: the adaptive-timestep constant `Cₑ` bounding how far one sample moves the coordinate — the
/// convergence/stability trade (Dabek 2004 §5.3; the recommended 0.25).
const TIMESTEP: f64 = 0.25;
/// Derived: the EWMA weight folding a new sample's relative error into the confidence estimate — the
/// same 0.25 responsiveness `Cₑ` uses (Dabek 2004 §5.3; hyperscale `error_decay`).
const ERROR_DECAY: f64 = 0.25;
/// Derived: the gravity that pulls coordinates gently toward the origin each update so the space does
/// not drift unboundedly (a small 0.01; hyperscale `gravity`).
const GRAVITY: f64 = 0.01;
/// Derived: how much of a sample's force feeds the height rather than the vector, so the shared last-hop
/// latency is learned separately from position (0.25; hyperscale `height_adjustment`).
const HEIGHT_ADJUSTMENT: f64 = 0.25;
/// Derived: the smoothing on the systematic per-node adjustment term (a small 0.05; hyperscale
/// `adjustment_smoothing`).
const ADJUSTMENT_SMOOTHING: f64 = 0.05;
/// Derived: the floor and ceiling on the confidence error, so a converged node keeps a little
/// responsiveness and a lost one does not swing without bound (0.05 and 10.0; hyperscale bounds).
const MIN_ERROR: f64 = 0.05;
/// Derived: the ceiling on the confidence error (see [`MIN_ERROR`]).
const MAX_ERROR: f64 = 10.0;
/// Format: the neutral half — an equal split of the move when both nodes are fully converged (a zero
/// total error), so neither coordinate dominates the update.
const NEUTRAL_WEIGHT: f64 = 0.5;

/// A node's Vivaldi coordinate engine: its own coordinate, relaxed toward each measured RTT.
pub struct CoordinateEngine {
  coordinate: NetworkCoordinate,
}

impl CoordinateEngine {
  /// A fresh engine for a node that has taken no samples (origin, full uncertainty).
  pub fn new() -> CoordinateEngine {
    CoordinateEngine {
      coordinate: NetworkCoordinate::origin(DIMENSIONS),
    }
  }

  /// This node's current coordinate (a copy — the engine owns the authoritative one).
  pub fn coordinate(&self) -> NetworkCoordinate {
    self.coordinate.clone()
  }

  /// The predicted round-trip time between `local` and `peer`: the Euclidean distance between their
  /// vectors plus both heights and adjustments, never negative. Symmetric in its arguments.
  pub fn estimate_rtt(local: &NetworkCoordinate, peer: &NetworkCoordinate) -> f64 {
    let distance = vector_distance(&local.vec, &peer.vec);
    let predicted = distance + local.height + peer.height + local.adjustment + peer.adjustment;
    predicted.max(0.0)
  }

  /// Predicts the round-trip time from this node to `peer`.
  pub fn predict(&self, peer: &NetworkCoordinate) -> f64 {
    CoordinateEngine::estimate_rtt(&self.coordinate, peer)
  }

  /// Relaxes this node's coordinate toward a measured `rtt` to `peer` (Vivaldi's spring update, Dabek
  /// 2004 §5.1 with the height/adjustment extensions): move along the line to the peer by the prediction
  /// error, weighted by relative confidence and bounded by the timestep; feed part of the error into the
  /// height and the adjustment; and fold the error magnitude into the confidence estimate. A
  /// non-positive `rtt` is ignored (an unusable sample). Returns the updated coordinate.
  pub fn update_with_rtt(&mut self, peer: &NetworkCoordinate, rtt: f64) -> NetworkCoordinate {
    if rtt <= 0.0 {
      return self.coordinate();
    }
    let predicted = CoordinateEngine::estimate_rtt(&self.coordinate, peer);
    let error = rtt - predicted;

    let distance = vector_distance(&self.coordinate.vec, &peer.vec);
    let unit = unit_vector(&self.coordinate.vec, &peer.vec, distance);
    let weight = confidence_weight(self.coordinate.error, peer.error);
    let step = TIMESTEP * weight;

    for (component, direction) in self.coordinate.vec.iter_mut().zip(unit.iter()) {
      *component += step * error * direction;
      *component *= 1.0 - GRAVITY;
    }

    let height_delta = HEIGHT_ADJUSTMENT * step * error;
    self.coordinate.height = (self.coordinate.height + height_delta).max(0.0);

    let adjustment_delta = ADJUSTMENT_SMOOTHING * error;
    self.coordinate.adjustment = clamp(self.coordinate.adjustment + adjustment_delta, -1.0, 1.0);

    let new_error = self.coordinate.error + ERROR_DECAY * (error.abs() - self.coordinate.error);
    self.coordinate.error = clamp(new_error, MIN_ERROR, MAX_ERROR);

    self.coordinate()
  }
}

impl Default for CoordinateEngine {
  fn default() -> CoordinateEngine {
    CoordinateEngine::new()
  }
}

/// The Euclidean distance between two coordinate vectors (shorter vector treated as zero-padded, so a
/// mismatch is safe rather than a panic).
fn vector_distance(left: &[f64], right: &[f64]) -> f64 {
  let mut sum = 0.0;
  let dimensions = left.len().max(right.len());
  for index in 0..dimensions {
    let a = left.get(index).copied().unwrap_or(0.0);
    let b = right.get(index).copied().unwrap_or(0.0);
    let delta = a - b;
    sum += delta * delta;
  }
  sum.sqrt()
}

/// The unit vector from `from` toward `to`. When the two coincide (zero distance) a small deterministic
/// nudge along the first axis breaks the degeneracy, so a coordinate never stalls exactly on a peer.
fn unit_vector(from: &[f64], to: &[f64], distance: f64) -> Vec<f64> {
  let dimensions = from.len().max(to.len());
  if distance <= f64::EPSILON {
    let mut nudge = vec![0.0; dimensions];
    if let Some(first) = nudge.first_mut() {
      *first = 1.0;
    }
    return nudge;
  }
  (0..dimensions)
    .map(|index| {
      let a = from.get(index).copied().unwrap_or(0.0);
      let b = to.get(index).copied().unwrap_or(0.0);
      (a - b) / distance
    })
    .collect()
}

/// The relative-confidence weight of the local error against the pair's total error: a node that is far
/// less confident than its peer moves more, one that is more confident moves less (Dabek 2004 §5.1). A
/// zero total (both fully converged) yields a neutral half.
fn confidence_weight(local_error: f64, peer_error: f64) -> f64 {
  let total = local_error + peer_error;
  if total <= f64::EPSILON {
    NEUTRAL_WEIGHT
  } else {
    local_error / total
  }
}

/// Clamps `value` into `[low, high]`.
fn clamp(value: f64, low: f64, high: f64) -> f64 {
  value.max(low).min(high)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A coordinate learns to predict a fixed peer's RTT: fed the same measured RTT repeatedly, the node's
  /// prediction converges toward it — the defining Vivaldi property.
  #[test]
  fn a_coordinate_converges_to_predict_a_peer() {
    // A peer fixed out along the first axis, fully confident.
    let mut peer = NetworkCoordinate::origin(DIMENSIONS);
    peer.vec[0] = 20.0;
    peer.error = MIN_ERROR;

    let target_rtt = 25.0;
    let mut engine = CoordinateEngine::new();
    for _ in 0..200 {
      engine.update_with_rtt(&peer, target_rtt);
    }

    let predicted = engine.predict(&peer);
    assert!(
      (predicted - target_rtt).abs() < target_rtt * 0.1,
      "prediction {predicted} converged near the measured RTT {target_rtt}"
    );
  }

  /// The estimate is symmetric and never negative.
  #[test]
  fn the_estimate_is_symmetric_and_nonnegative() {
    let mut a = NetworkCoordinate::origin(DIMENSIONS);
    a.vec[0] = 3.0;
    a.vec[1] = -4.0;
    a.height = 1.0;
    let mut b = NetworkCoordinate::origin(DIMENSIONS);
    b.vec[0] = -1.0;
    b.height = 2.0;

    let forward = CoordinateEngine::estimate_rtt(&a, &b);
    let backward = CoordinateEngine::estimate_rtt(&b, &a);
    assert!((forward - backward).abs() < f64::EPSILON, "symmetric");
    assert!(forward >= 0.0, "never negative");
  }

  /// A non-positive RTT is an unusable sample and leaves the coordinate untouched.
  #[test]
  fn a_non_positive_rtt_is_ignored() {
    let peer = NetworkCoordinate::origin(DIMENSIONS);
    let mut engine = CoordinateEngine::new();
    let before = engine.coordinate();
    engine.update_with_rtt(&peer, 0.0);
    engine.update_with_rtt(&peer, -5.0);
    assert_eq!(engine.coordinate(), before, "no sample, no movement");
  }

  /// Confidence improves (the error shrinks from full uncertainty) as consistent samples arrive.
  #[test]
  fn confidence_improves_with_consistent_samples() {
    let mut peer = NetworkCoordinate::origin(DIMENSIONS);
    peer.vec[0] = 10.0;
    peer.error = MIN_ERROR;

    let mut engine = CoordinateEngine::new();
    assert!(
      (engine.coordinate().error - 1.0).abs() < f64::EPSILON,
      "starts fully uncertain"
    );
    for _ in 0..100 {
      engine.update_with_rtt(&peer, 12.0);
    }
    assert!(
      engine.coordinate().error < 1.0,
      "error shrank from full uncertainty as predictions improved"
    );
  }
}
