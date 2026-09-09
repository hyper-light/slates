//! Observability spans (§4.14, D-23): the closed chokepoint-span roster and the three-id law. A span is
//! a chokepoint from a monotonic start to end carrying three distinct identities — the request id (for
//! exactly-once replay, [`RequestId`]), a trace id plus span id (which connect one unit of work across
//! bridges, rings, shards and holders), and an optional caused-by event identity (which connects causal
//! events). The three roles are distinct on purpose: the trace and caused-by identities are for
//! observation only and never authorize an effect (the A-9 correction — the authenticated consumer and
//! volume tags do, §4.13), so they are separate types from [`RequestId`], which routes and deduplicates.
//!
//! This module is the closed vocabulary — the roster ([`Chokepoint`]) and the identities
//! ([`SpanContext`]). The layers above it (owed): emitting a span asynchronously through the shard's
//! telemetry ring into the control sink, and the health plane refusing to serve until every chokepoint
//! has registered (§2.6). The types stay pure here (no wire encoding yet); the wire form arrives with
//! the emission path.

use crate::request::RequestId;

/// The closed roster of chokepoint spans (§4.14, "Span roster"): the nine points a span is opened at,
/// each with a monotonic start and end. The set is closed — a span is one of these and no other — so
/// the roster cannot drift; the design's "nine chokepoints" were once miscounted as seven (GAP-A9-12),
/// which an enum makes impossible. A doc-truth test pins the nine and their dotted names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chokepoint {
  /// A bridge call from arrival to reply (`bridge.request{op}`).
  BridgeRequest,
  /// A ring slot from read to reply written (`ring.request{kind}`).
  RingRequest,
  /// One verb on its owner shard, with no awaits inside (`shard.op{verb}`).
  ShardOp,
  /// One op-log record appended and published (`log.append{partition}`).
  LogAppend,
  /// One record or content put to its candidates, with the acknowledging count (`ship.record{object}`).
  ShipRecord,
  /// One configuration commit (`consensus.step{group}`).
  ConsensusStep,
  /// One chunk compressed or expanded (`archive.chunk{codec}`).
  ArchiveChunk,
  /// One landing entry (`land.entry{action}`).
  LandEntry,
  /// One increment judged (`merge.verdict`).
  MergeVerdict,
}

impl Chokepoint {
  /// The closed roster: every chokepoint span, in the design's roster order. The health plane checks
  /// that each has registered before it serves (§2.6); a doc-truth test pins the set and its names.
  pub const ALL: [Chokepoint; 9] = [
    Chokepoint::BridgeRequest,
    Chokepoint::RingRequest,
    Chokepoint::ShardOp,
    Chokepoint::LogAppend,
    Chokepoint::ShipRecord,
    Chokepoint::ConsensusStep,
    Chokepoint::ArchiveChunk,
    Chokepoint::LandEntry,
    Chokepoint::MergeVerdict,
  ];

  /// The span's dotted name — the stable vocabulary a consumer keys on. The `{op}`/`{verb}`/… label is
  /// the span's dimension, carried alongside, never part of the name.
  pub const fn name(self) -> &'static str {
    match self {
      Chokepoint::BridgeRequest => "bridge.request",
      Chokepoint::RingRequest => "ring.request",
      Chokepoint::ShardOp => "shard.op",
      Chokepoint::LogAppend => "log.append",
      Chokepoint::ShipRecord => "ship.record",
      Chokepoint::ConsensusStep => "consensus.step",
      Chokepoint::ArchiveChunk => "archive.chunk",
      Chokepoint::LandEntry => "land.entry",
      Chokepoint::MergeVerdict => "merge.verdict",
    }
  }
}

/// A trace identity (§4.14, three-id law): connects one unit of work across bridges, rings, shards and
/// holders. 128 bits so it does not collide across a fleet — the W3C Trace Context / OpenTelemetry trace
/// width, the modern form of Dapper's tracing (D-23 evidence). Observation only; it never authorizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(pub u128);

/// A span identity (§4.14): one span within a trace. 64 bits (the W3C Trace Context / OpenTelemetry
/// span-id width). Observation only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanId(pub u64);

/// A causal-event identity (§4.14): connects a span to the event that caused it (`caused_by`), distinct
/// from the trace that contains it. Observation only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CausedBy(pub u64);

/// The three-id law of a span (§4.14, D-23): the request identity that routes and deduplicates the work
/// ([`RequestId`], for exactly-once replay), the trace and span identities that connect the work across
/// boundaries, and the optional caused-by event identity. The identities have distinct roles and are
/// distinct types, so a trace field can never be mistaken for the authority a `RequestId` carries — the
/// A-9 correction: trace fields never authorize effects; the authenticated consumer and volume tags do
/// (§4.13). Consumer and volume are not identities here for exactly that reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanContext {
  /// The request this work serves, for exactly-once replay — it routes and deduplicates.
  pub request: RequestId,
  /// The trace connecting this work across bridges, rings, shards and holders (observation only).
  pub trace: TraceId,
  /// This span within the trace (observation only).
  pub span: SpanId,
  /// The event that caused this span, when there is one (observation only).
  pub caused_by: Option<CausedBy>,
}

#[cfg(test)]
mod tests {
  use super::Chokepoint;

  /// The chokepoint-span roster is closed (§4.14, GAP-A9-12): `ALL` is exactly the design's nine spans
  /// in order, with unique dotted names — so the roster the design calls "nine chokepoints" cannot be
  /// miscounted (it was once called seven) or drift silently through a free string.
  #[test]
  fn the_span_roster_is_closed_and_has_the_designs_nine_names() {
    // The design's roster (§4.14 "Span roster"), pinned here as the doc-truth: a change to the roster
    // must update this list, which is the point — a silent drift becomes a failing assertion.
    let expected = [
      "bridge.request",
      "ring.request",
      "shard.op",
      "log.append",
      "ship.record",
      "consensus.step",
      "archive.chunk",
      "land.entry",
      "merge.verdict",
    ];
    let names: Vec<&str> = Chokepoint::ALL.iter().map(|point| point.name()).collect();
    assert_eq!(
      names.as_slice(),
      expected,
      "ALL is the design's nine chokepoint spans, in roster order"
    );
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "the span names are unique");
    for name in &names {
      assert!(name.contains('.'), "a dotted span name: {name}");
    }
  }
}
