//! Observability spans (§4.14, D-23): the closed chokepoint-span roster and the three-id law. A span is
//! a chokepoint from a monotonic start to end carrying three distinct identities — the request id (for
//! exactly-once replay, [`RequestId`]), a trace id plus span id (which connect one unit of work across
//! bridges, rings, shards and holders), and an optional caused-by event identity (which connects causal
//! events). The three roles are distinct on purpose: the trace and caused-by identities are for
//! observation only and never authorize an effect (the A-9 correction — the authenticated consumer and
//! volume tags do, §4.13), so they are separate types from [`RequestId`], which routes and deduplicates.
//!
//! This module is the closed vocabulary — the roster ([`Chokepoint`]) and the identities
//! ([`SpanContext`]) — and the pure emission foundation: a completed [`Span`], the bounded shed-first
//! [`SpanSink`] the control shard collects into (every shed span counted explicitly), and the
//! [`ChokepointRegistry`] gate the health plane checks before it serves (§2.6 — it refuses until every
//! chokepoint in the roster has registered its emitter). Owed above this: the async delivery of a span
//! through the shard's telemetry ring into the control sink (in-process, by move — the `Control::Spawn`
//! path the cross-shard bridge queue already uses), and wiring the registry gate and the per-chokepoint
//! emit sites into the daemon boot. The types stay pure here — local delivery moves a span by value, so
//! no wire encoding is needed yet; the wire form arrives only if telemetry crosses nodes.

use std::collections::VecDeque;

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

  /// The chokepoint's index in the roster ([`Chokepoint::ALL`]) — its discriminant, since `ALL` lists
  /// the variants in declaration order. This keys the [`ChokepointRegistry`]'s per-chokepoint flags; a
  /// test pins that the index and the `ALL` position agree so the mapping cannot drift.
  pub const fn index(self) -> usize {
    self as usize
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

/// A completed chokepoint span (§4.14): a [`Chokepoint`] measured from a monotonic `start_ns` to
/// `end_ns`, carrying its three-id [`SpanContext`] and one content-free dimension code. A span is
/// emitted *after* it ends (the design's "a span is emitted after it ends"), so both timestamps are
/// set by the time it reaches the sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
  /// Which chokepoint (the closed roster).
  pub point: Chokepoint,
  /// The span's dimension as a bounded numeric code — the `{op}`/`{verb}`/`{partition}`/`{codec}`/…
  /// label of the design's roster, kept **content-free** (never a path, a name or file bytes; §4.14
  /// content-freedom) and bounded (an operation/verb/partition code, not a free string), so a span
  /// carries no user data by construction. The consumer maps the code back to a label per chokepoint.
  pub label: u32,
  /// The three identities: the request (for exactly-once replay), the trace and span (which connect
  /// the work across boundaries), and the optional caused-by. Trace fields never authorize (§4.14).
  pub context: SpanContext,
  /// The monotonic start (ns).
  pub start_ns: u64,
  /// The monotonic end (ns); always set, because a span is emitted only after it ends.
  pub end_ns: u64,
}

impl Span {
  /// The span's duration, saturating at zero if the monotonic clock somehow ran backwards (it must
  /// not, but an observability type never underflows on a bad sample — it reports zero).
  pub const fn duration_ns(&self) -> u64 {
    self.end_ns.saturating_sub(self.start_ns)
  }
}

/// A bounded, shed-first telemetry sink (§4.14): the control shard collects emitted spans here. It is a
/// ring of a fixed capacity — when full, the **oldest** span is shed to admit the newest (telemetry is
/// the shed-first class, §4.14; recent activity is the more useful to keep, and the structure never
/// grows without bound, CLAUDE §2 ban 8) — and **every shed span is counted**, so a consumer sees loss
/// explicitly ("bounded rings report dropped spans", §4.14) rather than a silent gap. Registration does
/// not prove live telemetry; this held count and drop count are the proof of liveness and of loss.
#[derive(Clone, Debug)]
pub struct SpanSink {
  /// The held spans, oldest at the front (a ring: the front is shed first when full).
  spans: VecDeque<Span>,
  /// The bound. Derived at the call site from the telemetry budget; never a magic number here.
  capacity: usize,
  /// Spans shed since creation — the explicit loss signal, never reset except by a new sink.
  dropped: u64,
}

impl SpanSink {
  /// A sink bounded to `capacity` spans. The bound is the caller's (a derived value from the telemetry
  /// budget); a capacity of zero is a valid degenerate sink that sheds and counts every span.
  pub fn with_capacity(capacity: usize) -> SpanSink {
    SpanSink {
      spans: VecDeque::with_capacity(capacity),
      capacity,
      dropped: 0,
    }
  }

  /// Emits one completed span. When the sink is full, the oldest span is shed (and counted) to make
  /// room for the newest — so emission never blocks the work that produced it and never grows the sink.
  pub fn emit(&mut self, span: Span) {
    if self.spans.len() == self.capacity {
      // At capacity: shed the oldest (or this span, when the bound is zero) and count the loss.
      if self.spans.pop_front().is_none() {
        self.dropped = self.dropped.saturating_add(1);
        return;
      }
      self.dropped = self.dropped.saturating_add(1);
    }
    self.spans.push_back(span);
  }

  /// The spans held now, oldest first.
  pub fn spans(&self) -> impl Iterator<Item = &Span> {
    self.spans.iter()
  }

  /// How many spans are held now (at most the capacity).
  pub fn len(&self) -> usize {
    self.spans.len()
  }

  /// Whether the sink holds no spans.
  pub fn is_empty(&self) -> bool {
    self.spans.is_empty()
  }

  /// How many spans have been shed since creation — the explicit loss signal (§4.14). It never resets
  /// except by dropping the sink (a fresh daemon generation starts a fresh count).
  pub fn dropped(&self) -> u64 {
    self.dropped
  }

  /// Takes the held spans, leaving the sink empty; the drop count is retained (loss is not forgotten
  /// by reading). For a consumer that drains the sink when it reports.
  pub fn drain(&mut self) -> Vec<Span> {
    self.spans.drain(..).collect()
  }
}

/// The chokepoint registration gate (§2.6, §4.14): the health plane refuses to serve until every
/// chokepoint in the roster has registered its emitter, so a daemon never serves with a silently
/// missing span source (design line 517, "refuses to serve until every chokepoint has registered — an
/// unregistered emitter fails"). It is **fail-closed**: a fresh registry's gate is shut, and it opens
/// only once all nine are present. Registration proves the emitter *exists*, not that it is *live* (a
/// registered emitter may be idle); liveness is the sink's business, not the gate's.
#[derive(Clone, Debug, Default)]
pub struct ChokepointRegistry {
  /// One flag per chokepoint, keyed by [`Chokepoint::index`] (roster order).
  registered: [bool; Chokepoint::ALL.len()],
}

impl ChokepointRegistry {
  /// A fresh registry with nothing registered — the gate is shut (fail-closed).
  pub fn new() -> ChokepointRegistry {
    ChokepointRegistry::default()
  }

  /// Registers one chokepoint's emitter. Registering an already-registered chokepoint is idempotent.
  pub fn register(&mut self, point: Chokepoint) {
    self.registered[point.index()] = true;
  }

  /// Whether a chokepoint has registered.
  pub fn is_registered(&self, point: Chokepoint) -> bool {
    self.registered[point.index()]
  }

  /// Whether every chokepoint in the roster has registered — the gate is open and the health plane
  /// may serve. The daemon checks this at boot before it announces itself (§2.6).
  pub fn is_ready(&self) -> bool {
    self.registered.iter().all(|&flag| flag)
  }

  /// The chokepoints that have not registered yet, in roster order — what a not-ready refusal names,
  /// so an operator sees exactly which emitter is missing rather than a bare "not ready".
  pub fn missing(&self) -> Vec<Chokepoint> {
    Chokepoint::ALL
      .iter()
      .copied()
      .filter(|point| !self.registered[point.index()])
      .collect()
  }
}

#[cfg(test)]
mod tests {
  use super::{
    Chokepoint, ChokepointRegistry, RequestId, Span, SpanContext, SpanId, SpanSink, TraceId,
  };

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

  /// A chokepoint's `index` is its position in `ALL` (§4.14): the [`ChokepointRegistry`] keys its flags
  /// by `index`, so if a variant's discriminant and its `ALL` position ever disagreed, a registration
  /// would set the wrong flag. This pins them together, so that reordering cannot silently corrupt the
  /// gate.
  #[test]
  fn a_chokepoints_index_is_its_roster_position() {
    for (position, point) in Chokepoint::ALL.iter().enumerate() {
      assert_eq!(
        point.index(),
        position,
        "{} indexes its ALL slot",
        point.name()
      );
    }
  }

  /// The registration gate is fail-closed and opens only when all nine chokepoints have registered
  /// (§2.6 "refuses to serve until every chokepoint has registered"). Do: register eight of nine, then
  /// the ninth. Expect: the gate stays shut and names exactly the missing chokepoint until the last
  /// registers, then opens.
  #[test]
  fn the_registration_gate_opens_only_when_every_chokepoint_has_registered() {
    let mut registry = ChokepointRegistry::new();
    assert!(
      !registry.is_ready(),
      "a fresh registry's gate is shut (fail-closed)"
    );
    assert_eq!(
      registry.missing(),
      Chokepoint::ALL.to_vec(),
      "every chokepoint is missing at first"
    );
    // Register all but the last of the roster; the gate must stay shut and name the one that is left.
    let (last, rest) = Chokepoint::ALL
      .split_last()
      .expect("the roster is non-empty");
    for point in rest {
      registry.register(*point);
    }
    assert!(!registry.is_ready(), "eight of nine keeps the gate shut");
    assert_eq!(
      registry.missing(),
      vec![*last],
      "the one unregistered chokepoint is named"
    );
    assert!(!registry.is_registered(*last));
    registry.register(*last);
    assert!(registry.is_ready(), "all nine opens the gate");
    assert!(
      registry.missing().is_empty(),
      "nothing is missing once the gate is open"
    );
  }

  /// The telemetry sink is bounded, keeps the most recent spans (a ring), and counts every shed span
  /// explicitly (§4.14 "bounded rings report dropped spans"). Do: emit five spans into a sink of three.
  /// Expect: it holds the three most recent (oldest first), counts the two it shed, and draining keeps
  /// the loss count. The drop counter is the non-vacuity witness — a silently-lossless sink would fail.
  #[test]
  fn the_sink_is_bounded_keeps_the_most_recent_and_counts_every_shed_span() {
    let capacity = 3;
    let mut sink = SpanSink::with_capacity(capacity);
    assert!(sink.is_empty());
    for label in 0..5u32 {
      sink.emit(span_labeled(label));
    }
    assert_eq!(sink.len(), capacity, "the sink never grows past its bound");
    assert_eq!(
      sink.dropped(),
      2,
      "the two oldest spans were shed and counted"
    );
    let held: Vec<u32> = sink.spans().map(|span| span.label).collect();
    assert_eq!(
      held,
      vec![2, 3, 4],
      "the three most recent survived, oldest first"
    );
    let drained = sink.drain();
    assert_eq!(drained.len(), capacity, "drain returns the held spans");
    assert!(sink.is_empty(), "the sink is empty after a drain");
    assert_eq!(sink.dropped(), 2, "draining does not reset the loss signal");
  }

  /// A span measures its own duration and never underflows on a backwards clock (§4.14): end − start,
  /// saturating to zero. Do: a forward span, then one whose end precedes its start. Expect: the real
  /// duration, then zero (not a wrapped huge value).
  #[test]
  fn a_span_measures_its_own_duration_and_saturates_a_backwards_clock() {
    let span = Span {
      point: Chokepoint::ShardOp,
      label: 7,
      context: a_context(),
      start_ns: 100,
      end_ns: 250,
    };
    assert_eq!(span.duration_ns(), 150);
    let backwards = Span {
      start_ns: 250,
      end_ns: 100,
      ..span
    };
    assert_eq!(
      backwards.duration_ns(),
      0,
      "a backwards clock saturates to zero"
    );
  }

  /// A span with the given dimension code; the ids are fixed because the sink tests are about ordering
  /// and bounds, not identity.
  fn span_labeled(label: u32) -> Span {
    Span {
      point: Chokepoint::ShardOp,
      label,
      context: a_context(),
      start_ns: 0,
      end_ns: 0,
    }
  }

  /// A fixed span context for the tests.
  fn a_context() -> SpanContext {
    SpanContext {
      request: RequestId {
        client: 1,
        sequence: 1,
      },
      trace: TraceId(1),
      span: SpanId(1),
      caused_by: None,
    }
  }
}
