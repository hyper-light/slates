//! Observability spans (§4.14, D-23): the closed chokepoint-span registry and the three-id law. A span is
//! a chokepoint from a monotonic start to end carrying three distinct identities — the request id (for
//! exactly-once replay, [`RequestId`]), a trace id plus span id (which connect one unit of work across
//! bridges, rings, shards and holders), and a caused-by identity (which connects causal events). The
//! three roles are distinct on purpose: the trace and caused-by identities are for observation only and
//! never authorize an effect (the A-9 correction — the authenticated consumer and volume tags do, §4.13),
//! so they are separate types from [`RequestId`], which routes and deduplicates, and they are distinct in
//! *value* too: a trace id is opened by the shard that admits the work, never derived from the request
//! word, so neither identity can be forged from the other (GAP-A9-12 "request-vs-trace causation").
//!
//! This module is the closed vocabulary — the registry ([`Chokepoint`]: name, dimension, what absence
//! means, expected producer, observer, freshness horizon; the health-signal registry, `HealthSignal`,
//! lives beside it in `slates-ipc` and shares [`AbsenceIs`]) — and the pure emission foundation. **Causation is enforced by the types**: a [`Span`] is
//! built only by ending an [`OpenSpan`], and an `OpenSpan` is opened only by a shard's [`Tracer`], which
//! needs a [`RequestId`] to open a root (a new trace at an entry point) and a parent [`SpanContext`] to
//! open a child (same request, same trace, [`Cause::Span`] naming the parent). A span whose cause existed
//! but was not carried across a boundary is opened *unlinked* and says so ([`Cause::Missing`]) — the
//! "missing causal links" the design has rings report explicitly, never a silent root. The bounded
//! shed-first [`SpanSink`] counts every shed span and marks each drain with the loss before it, and the
//! [`ChokepointRegistry`] gate refuses to serve until every chokepoint has registered (§2.6). The types
//! stay pure: local delivery moves a span by value; the operator export (`slates-ipc`'s `TelemetryReport`)
//! is the wire form.
//!
//! Evidence: 128-bit trace and 64-bit span ids are the W3C Trace Context / OpenTelemetry widths, the
//! modern form of Dapper [A: Sigelman et al., 2010] (D-23); the shed-first telemetry class and the
//! per-shard rings are §4.14's own words.

use std::collections::VecDeque;

use crate::Wire;
use crate::request::RequestId;

/// The closed roster of chokepoint spans (§4.14, "Span roster"): the nine points a span is opened at,
/// each with a monotonic start and end. The set is closed — a span is one of these and no other — so
/// the roster cannot drift; the design's "nine chokepoints" were once miscounted as seven (GAP-A9-12),
/// which an enum makes impossible. A doc-truth test pins the nine, their dotted names and their
/// dimensions against the design's own roster sentence, and the registry table it renders against
/// `docs/wip/observability.md`.
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

/// What a signal's absence means (§4.14, A-9): a missing sample is never silently read as "healthy".
/// A value that is absent — never measured, or not fresh within its horizon — carries this so a consumer
/// knows whether the gap is benign or a fault. Shared by the chokepoint registry here and the health
/// signal registry in `slates-ipc`, so both surfaces speak one vocabulary.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbsenceIs {
  /// The value is simply not known: nothing has measured it, or nothing has exercised the path within
  /// its horizon (an idle chokepoint, a fresh daemon's replay time before any replay). Not a fault.
  Unknown,
  /// The value should be present; its absence means a producer that ought to be reporting is not,
  /// which is itself a degradation, not a healthy zero.
  Degraded,
}

impl AbsenceIs {
  /// The lowercase name the CLI and MCP surfaces render (`absent/unknown`).
  pub const fn name(self) -> &'static str {
    match self {
      AbsenceIs::Unknown => "unknown",
      AbsenceIs::Degraded => "degraded",
    }
  }
}

/// What is expected to exercise a chokepoint (§4.14 "expected producer"): the report says whether that
/// producer runs on this host, so an operator can tell "absent because nothing here can produce it" from
/// "absent because idle".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Producer {
  /// A client's verb on its ring (every host).
  ClientVerb,
  /// The kernel's filesystem bridge (a host that serves a mount).
  Bridge,
  /// A landing under a grant (a host that can write a target, §4.15).
  Landing,
  /// The record plane shipping to candidate holders (a fleet node with `f ≥ 1`, §4.8).
  Replication,
  /// The configuration group committing (a fleet node, §4.8).
  Consensus,
  /// The archive codec compressing or expanding a chunk (§4.10; the codec pass is owed, so no host yet).
  Archive,
}

impl Producer {
  /// The producer's name in the registry table and the exported report.
  pub const fn name(self) -> &'static str {
    match self {
      Producer::ClientVerb => "client verb",
      Producer::Bridge => "filesystem bridge",
      Producer::Landing => "landing under grant",
      Producer::Replication => "record replication (f ≥ 1)",
      Producer::Consensus => "configuration group",
      Producer::Archive => "archive codec",
    }
  }
}

/// Who observes a signal (§4.14 "observer"): every chokepoint span is recorded by the shard that ran
/// the work, into that shard's own ring — self-observed. A host-observed signal (the anchor watching the
/// daemon) is a health signal, not a span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observer {
  /// The owning shard's telemetry ring, on the shard's own thread.
  OwnerShard,
}

impl Observer {
  /// The observer's name in the registry table.
  pub const fn name(self) -> &'static str {
    match self {
      Observer::OwnerShard => "owner shard (self-observed)",
    }
  }
}

/// A signal's freshness horizon (§4.14 "freshness horizon"): how old the newest sample may be and still
/// describe the present. Beyond it the export reports typed absence, never the stale value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Horizon {
  /// The operator's failover SLO (the lease term, D-16): the one horizon the operator has declared for
  /// "is this still alive?", so activity older than it is not evidence of liveness.
  FailoverSlo,
}

impl Horizon {
  /// The horizon's name in the registry table.
  pub const fn name(self) -> &'static str {
    match self {
      Horizon::FailoverSlo => "failover SLO",
    }
  }
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

  /// The span's dimension name — the `{…}` label of the design's roster (`bridge.request{op}`), whose
  /// value a [`Span`] carries as a content-free numeric code. Empty for a span with no dimension
  /// (`merge.verdict`). Pinned against the design's roster sentence by the doc-truth test.
  pub const fn dimension(self) -> &'static str {
    match self {
      Chokepoint::BridgeRequest => "op",
      Chokepoint::RingRequest => "kind",
      Chokepoint::ShardOp => "verb",
      Chokepoint::LogAppend => "partition",
      Chokepoint::ShipRecord => "object",
      Chokepoint::ConsensusStep => "group",
      Chokepoint::ArchiveChunk => "codec",
      Chokepoint::LandEntry => "action",
      Chokepoint::MergeVerdict => "",
    }
  }

  /// What this chokepoint's absence means (§4.14, A-9). Every chokepoint is event-driven — a span
  /// exists only when work crossed the point — so no span within the horizon means "not known to be
  /// live" (idle, or nothing here can produce it: see [`Producer`]), never a fault by itself:
  /// [`AbsenceIs::Unknown`]. `Degraded` is reserved for a chokepoint whose producer runs on a cadence,
  /// whose silence would prove the emitter dead; none does today, and a new one must decide here.
  pub const fn absence(self) -> AbsenceIs {
    match self {
      Chokepoint::BridgeRequest
      | Chokepoint::RingRequest
      | Chokepoint::ShardOp
      | Chokepoint::LogAppend
      | Chokepoint::ShipRecord
      | Chokepoint::ConsensusStep
      | Chokepoint::ArchiveChunk
      | Chokepoint::LandEntry
      | Chokepoint::MergeVerdict => AbsenceIs::Unknown,
    }
  }

  /// What is expected to exercise this chokepoint (§4.14 "expected producer").
  pub const fn producer(self) -> Producer {
    match self {
      Chokepoint::BridgeRequest => Producer::Bridge,
      Chokepoint::RingRequest
      | Chokepoint::ShardOp
      | Chokepoint::LogAppend
      | Chokepoint::MergeVerdict => Producer::ClientVerb,
      Chokepoint::LandEntry => Producer::Landing,
      Chokepoint::ShipRecord => Producer::Replication,
      Chokepoint::ConsensusStep => Producer::Consensus,
      Chokepoint::ArchiveChunk => Producer::Archive,
    }
  }

  /// Who records this chokepoint's spans (§4.14 "observer").
  pub const fn observer(self) -> Observer {
    match self {
      Chokepoint::BridgeRequest
      | Chokepoint::RingRequest
      | Chokepoint::ShardOp
      | Chokepoint::LogAppend
      | Chokepoint::ShipRecord
      | Chokepoint::ConsensusStep
      | Chokepoint::ArchiveChunk
      | Chokepoint::LandEntry
      | Chokepoint::MergeVerdict => Observer::OwnerShard,
    }
  }

  /// How old this chokepoint's newest span may be and still count as current (§4.14 "freshness
  /// horizon"); older, the export reports typed absence, not the stale age as a live value.
  pub const fn horizon(self) -> Horizon {
    match self {
      Chokepoint::BridgeRequest
      | Chokepoint::RingRequest
      | Chokepoint::ShardOp
      | Chokepoint::LogAppend
      | Chokepoint::ShipRecord
      | Chokepoint::ConsensusStep
      | Chokepoint::ArchiveChunk
      | Chokepoint::LandEntry
      | Chokepoint::MergeVerdict => Horizon::FailoverSlo,
    }
  }

  /// The chokepoint's index in the roster ([`Chokepoint::ALL`]) — its discriminant, since `ALL` lists
  /// the variants in declaration order. This keys the [`ChokepointRegistry`]'s per-chokepoint flags and
  /// is the code an exported span record names its chokepoint by; a test pins that the index and the
  /// `ALL` position agree so the mapping cannot drift.
  pub const fn index(self) -> usize {
    self as usize
  }

  /// The chokepoint at a roster index, or none past the roster (a hostile or stale code).
  pub fn from_index(index: usize) -> Option<Chokepoint> {
    Chokepoint::ALL.get(index).copied()
  }

  /// The registry as the Markdown table `docs/wip/observability.md` carries — one row per chokepoint in
  /// roster order, every column a registry method — so the document is generated from the code and the
  /// doc-truth test fails the moment either drifts.
  pub fn registry_table() -> String {
    let mut table = String::from(
      "| Chokepoint | Dimension | Absence means | Expected producer | Observer | Freshness horizon |\n|---|---|---|---|---|---|\n",
    );
    for point in Chokepoint::ALL {
      let dimension = if point.dimension().is_empty() {
        "—".to_owned()
      } else {
        format!("`{{{}}}`", point.dimension())
      };
      table.push_str(&format!(
        "| `{}` | {dimension} | {} | {} | {} | {} |\n",
        point.name(),
        point.absence().name(),
        point.producer().name(),
        point.observer().name(),
        point.horizon().name(),
      ));
    }
    table
  }
}

/// A trace identity (§4.14, three-id law): connects one unit of work across bridges, rings, shards and
/// holders. 128 bits so it does not collide across a fleet — the W3C Trace Context / OpenTelemetry trace
/// width, the modern form of Dapper's tracing (D-23 evidence). Opened by the admitting shard's
/// [`Tracer`], never derived from a request id: observation only; it never authorizes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(pub u128);

/// A span identity (§4.14): one span within a trace. 64 bits (the W3C Trace Context / OpenTelemetry
/// span-id width), unique within the daemon because the shard's partition sits in its high bits.
/// Observation only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanId(pub u64);

/// What caused a span (§4.14 "`caused_by` connects causal events"). Distinct from the trace that contains
/// the span: the trace says *which* work, the cause says *what led to* this step of it. Observation only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
  /// The span opened its trace at an entry point — a ring slot read, a bridge call arriving. Nothing in
  /// the daemon caused it; the client or the kernel did.
  Root,
  /// The span was caused by this span of the same trace (its parent).
  Span(SpanId),
  /// A cause existed but was not carried across the boundary the work crossed (a verb forwarded to
  /// another node over the fleet transport, whose envelope carries no trace context yet). The link is
  /// declared missing rather than the span passed off as a root — the "missing causal links" §4.14 has
  /// bounded rings report explicitly.
  Missing,
}

/// The three-id law of a span (§4.14, D-23): the request identity that routes and deduplicates the work
/// ([`RequestId`], for exactly-once replay), the trace and span identities that connect the work across
/// boundaries, and the cause. The identities have distinct roles and are distinct types, so a trace field
/// can never be mistaken for the authority a `RequestId` carries — the A-9 correction: trace fields never
/// authorize effects; the authenticated consumer and volume tags do (§4.13). Consumer and volume are not
/// identities here for exactly that reason. The fields are private: a context is made only by a
/// [`Tracer`] (a root from a request, a child from its parent), so a child cannot carry a different
/// request or trace than its cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanContext {
  /// The request this work serves, for exactly-once replay — it routes and deduplicates.
  request: RequestId,
  /// The trace connecting this work across bridges, rings, shards and holders (observation only).
  trace: TraceId,
  /// This span within the trace (observation only).
  span: SpanId,
  /// What caused this span (observation only).
  cause: Cause,
}

impl SpanContext {
  /// The request this work serves (the replay identity).
  pub const fn request(&self) -> RequestId {
    self.request
  }

  /// The trace this span belongs to.
  pub const fn trace(&self) -> TraceId {
    self.trace
  }

  /// This span's own identity.
  pub const fn span(&self) -> SpanId {
    self.span
  }

  /// What caused this span.
  pub const fn cause(&self) -> Cause {
    self.cause
  }
}

/// A span that has started and not yet ended (§4.14): its chokepoint, its three-id context and its
/// monotonic start. Ending it ([`OpenSpan::end`]) is the only way to make a [`Span`], so every emitted
/// span was opened by a [`Tracer`] with the identities the type demands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenSpan {
  point: Chokepoint,
  context: SpanContext,
  start_ns: u64,
}

impl OpenSpan {
  /// The span's three-id context, for opening children within it.
  pub const fn context(&self) -> SpanContext {
    self.context
  }

  /// The chokepoint this span measures.
  pub const fn point(&self) -> Chokepoint {
    self.point
  }

  /// The monotonic start (ns).
  pub const fn start_ns(&self) -> u64 {
    self.start_ns
  }

  /// Ends the span at `end_ns` with its content-free dimension code, yielding the completed [`Span`] to
  /// emit. A span is emitted only after it ends (the design's "a span is emitted after it ends").
  pub const fn end(self, label: u32, end_ns: u64) -> Span {
    Span {
      point: self.point,
      label,
      context: self.context,
      start_ns: self.start_ns,
      end_ns,
    }
  }
}

/// A completed chokepoint span (§4.14): a [`Chokepoint`] measured from a monotonic `start_ns` to
/// `end_ns`, carrying its three-id [`SpanContext`] and one content-free dimension code. Made only by
/// [`OpenSpan::end`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
  point: Chokepoint,
  label: u32,
  context: SpanContext,
  start_ns: u64,
  end_ns: u64,
}

impl Span {
  /// Which chokepoint (the closed roster).
  pub const fn point(&self) -> Chokepoint {
    self.point
  }

  /// The span's dimension as a bounded numeric code — the `{op}`/`{verb}`/`{partition}`/`{codec}`/…
  /// label of the design's roster, kept **content-free** (never a path, a name or file bytes; §4.14
  /// content-freedom) and bounded (an operation/verb/partition code, not a free string), so a span
  /// carries no user data by construction. The consumer maps the code back to a label per chokepoint.
  pub const fn label(&self) -> u32 {
    self.label
  }

  /// The three identities: the request (for exactly-once replay), the trace and span (which connect
  /// the work across boundaries), and the cause. Trace fields never authorize (§4.14).
  pub const fn context(&self) -> SpanContext {
    self.context
  }

  /// The monotonic start (ns).
  pub const fn start_ns(&self) -> u64 {
    self.start_ns
  }

  /// The monotonic end (ns); always set, because a span is emitted only after it ends.
  pub const fn end_ns(&self) -> u64 {
    self.end_ns
  }

  /// The span's duration, saturating at zero if the monotonic clock somehow ran backwards (it must
  /// not, but an observability type never underflows on a bad sample — it reports zero).
  pub const fn duration_ns(&self) -> u64 {
    self.end_ns.saturating_sub(self.start_ns)
  }
}

/// Format: the bits of a span or trace counter below the partition field — 48, so a shard's counter
/// runs 2^48 spans (8.9 years at one span per microsecond) before it would touch the partition bits; it
/// is masked to that width so it never can.
const COUNTER_BITS: u32 = 48;

/// One shard's opener of spans (§4.14): a per-shard monotonic counter names the traces it opens and the
/// spans it opens, folded with the node and partition so the ids are unique daemon-wide (span) and
/// fleet-wide (trace) with no coordination — D-14's "ids route to owners" applied to telemetry. It is the
/// only source of a [`SpanContext`]: a root needs the [`RequestId`] it serves, a child needs the context
/// that caused it, and an unlinked span declares its cause missing. Per shard and thread-local in the
/// shard's state, so no atomics and no lock (R2, D-7).
#[derive(Clone, Debug)]
pub struct Tracer {
  /// The node this shard runs on (the member id), the high 64 bits of every trace id it opens.
  node: u64,
  /// The shard's partition, the field above the counter in every span and trace id.
  partition: u16,
  /// The next counter value; monotonic, never reset except by a restart.
  next: u64,
}

impl Tracer {
  /// A tracer for one shard of one node.
  pub const fn new(node: u64, partition: u16) -> Tracer {
    Tracer {
      node,
      partition,
      next: 1,
    }
  }

  /// The next counter value, masked to its width.
  fn take(&mut self) -> u64 {
    let counter = self.next & ((1u64 << COUNTER_BITS) - 1);
    self.next = self.next.wrapping_add(1);
    counter
  }

  /// A span id: the partition above the counter, so two shards never mint the same id.
  fn span_id(&mut self) -> SpanId {
    SpanId((u64::from(self.partition) << COUNTER_BITS) | self.take())
  }

  /// A trace id: the node, the partition, then the counter, so two nodes or two shards never mint the
  /// same trace.
  fn trace_id(&mut self) -> TraceId {
    TraceId(
      (u128::from(self.node) << u64::BITS)
        | (u128::from(self.partition) << COUNTER_BITS)
        | u128::from(self.take()),
    )
  }

  /// Opens a root span: a new trace for `request`, at an entry point (a ring slot read, a bridge call).
  /// Its cause is [`Cause::Root`]. A retry of the same request opens a new trace — the request identity
  /// is for replay and outlives any one trace; the two have different lifetimes (§4.9 "Trace context").
  pub fn open_root(&mut self, request: RequestId, point: Chokepoint, start_ns: u64) -> OpenSpan {
    let trace = self.trace_id();
    let span = self.span_id();
    OpenSpan {
      point,
      context: SpanContext {
        request,
        trace,
        span,
        cause: Cause::Root,
      },
      start_ns,
    }
  }

  /// Opens a span caused by `cause`: the same request and trace, a fresh span id, and
  /// [`Cause::Span`] naming the parent — the type makes a child that disagrees with its cause impossible.
  pub fn open_within(&mut self, cause: &SpanContext, point: Chokepoint, start_ns: u64) -> OpenSpan {
    let span = self.span_id();
    OpenSpan {
      point,
      context: SpanContext {
        request: cause.request,
        trace: cause.trace,
        span,
        cause: Cause::Span(cause.span),
      },
      start_ns,
    }
  }

  /// Opens a span whose cause existed but was not carried to this shard (a verb forwarded over a boundary
  /// that carries no trace context): a new trace for `request`, with the link declared
  /// [`Cause::Missing`] so the gap is reported, never disguised as a root.
  pub fn open_unlinked(
    &mut self,
    request: RequestId,
    point: Chokepoint,
    start_ns: u64,
  ) -> OpenSpan {
    let trace = self.trace_id();
    let span = self.span_id();
    OpenSpan {
      point,
      context: SpanContext {
        request,
        trace,
        span,
        cause: Cause::Missing,
      },
      start_ns,
    }
  }
}

/// A bounded, shed-first telemetry ring (§4.14): the shard collects its emitted spans here. It has a
/// fixed capacity — when full, the **oldest** span is shed to admit the newest (telemetry is the
/// shed-first class, §4.14; recent activity is the more useful to keep, and the structure never grows
/// without bound, CLAUDE §2 ban 8) — and **every shed span is counted**, so a consumer sees loss
/// explicitly ("bounded rings report dropped spans", §4.14) rather than a silent gap. Registration does
/// not prove live telemetry; the held count, the drain and the drop count are the proof of liveness and
/// of loss.
#[derive(Clone, Debug)]
pub struct SpanSink {
  /// The held spans, oldest at the front (a ring: the front is shed first when full).
  spans: VecDeque<Span>,
  /// The bound. Derived at the call site from the telemetry budget; never a magic number here.
  capacity: usize,
  /// Spans shed since creation — the explicit loss signal, never reset except by a new sink.
  dropped: u64,
  /// Spans shed since the last drain — the loss marker the next drain carries, so a reader of a batch
  /// knows how much was lost before its first span.
  shed_since_drain: u64,
}

/// What one bounded drain of a [`SpanSink`] yields: the spans (oldest first), the typed loss marker for
/// the batch, and how many spans the bound left in the ring for the next drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drained {
  /// The spans taken, oldest first.
  pub spans: Vec<Span>,
  /// Spans shed between the previous drain and this one — lost before the first span here. The loss
  /// marker: never silent, never folded into a count a reader could miss.
  pub shed_before: u64,
  /// Spans still held after this drain (the bound stopped short of emptying the ring); a consumer that
  /// wants them drains again.
  pub remaining: usize,
}

impl SpanSink {
  /// A sink bounded to `capacity` spans. The bound is the caller's (a derived value from the telemetry
  /// budget); a capacity of zero is a valid degenerate sink that sheds and counts every span.
  pub fn with_capacity(capacity: usize) -> SpanSink {
    SpanSink {
      spans: VecDeque::with_capacity(capacity),
      capacity,
      dropped: 0,
      shed_since_drain: 0,
    }
  }

  /// Emits one completed span. When the sink is full, the oldest span is shed (and counted) to make
  /// room for the newest — so emission never blocks the work that produced it and never grows the sink.
  pub fn emit(&mut self, span: Span) {
    if self.spans.len() == self.capacity {
      // At capacity: shed the oldest (or this span, when the bound is zero) and count the loss.
      let shed_this_one = self.spans.pop_front().is_none();
      self.count_shed(1);
      if shed_this_one {
        return;
      }
    }
    self.spans.push_back(span);
  }

  /// Counts `n` shed spans in both the lifetime total and the marker for the next drain.
  fn count_shed(&mut self, n: u64) {
    self.dropped = self.dropped.saturating_add(n);
    self.shed_since_drain = self.shed_since_drain.saturating_add(n);
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

  /// Folds in `n` spans that were shed *before* reaching this sink — by a bounded collector a lower
  /// crate recorded into (a large landing's per-entry spans), whose overflow the server carries here so
  /// the total loss stays one honest number (§4.14 "bounded rings report dropped spans"), not two.
  pub fn record_dropped(&mut self, n: u64) {
    self.count_shed(n);
  }

  /// Takes up to `max` of the held spans, oldest first, with the loss marker for the batch (the spans
  /// shed since the previous drain) and the count left behind. The lifetime drop count is retained (loss
  /// is not forgotten by reading); the marker resets, so each batch reports its own loss exactly once.
  pub fn drain_up_to(&mut self, max: usize) -> Drained {
    let taken = max.min(self.spans.len());
    let spans: Vec<Span> = self.spans.drain(..taken).collect();
    let shed_before = std::mem::take(&mut self.shed_since_drain);
    Drained {
      spans,
      shed_before,
      remaining: self.spans.len(),
    }
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
    AbsenceIs, Cause, Chokepoint, ChokepointRegistry, RequestId, Span, SpanSink, Tracer,
  };

  /// The design's roster paragraph (§4.14 "Span roster"), read from the design document itself.
  const DESIGN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/wip/SLATES_DESIGN.md"
  );
  /// The living observability record whose registry table is generated from this module.
  const RECORD: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/wip/observability.md"
  );
  /// The markers around the generated chokepoint table in the record.
  const TABLE_BEGIN: &str = "<!-- chokepoints:begin -->\n";
  const TABLE_END: &str = "<!-- chokepoints:end -->";

  /// The `name{dimension}` tokens of the design's "*Span roster.*" sentence, in the order the design
  /// lists them, and the count word the sentence opens with ("Nine chokepoints").
  fn design_roster() -> (String, Vec<(String, String)>) {
    let design = std::fs::read_to_string(DESIGN).expect("the design document is readable");
    let sentence = design
      .lines()
      .find_map(|line| line.strip_prefix("*Span roster.* "))
      .expect("the design has a `*Span roster.*` paragraph");
    let count_word = sentence
      .split_whitespace()
      .next()
      .expect("the roster sentence opens with its count")
      .to_lowercase();
    let mut tokens = Vec::new();
    let mut rest = sentence;
    while let Some(start) = rest.find('`') {
      let after = &rest[start + 1..];
      let Some(end) = after.find('`') else { break };
      let token = &after[..end];
      // A roster token is `name{dimension}` or a bare `name`; anything else in backticks (`caused_by`,
      // a section reference) has no dot and is skipped.
      if let Some(dot) = token.find('.')
        && dot > 0
        && !token.contains(' ')
      {
        let (name, dimension) = match token.split_once('{') {
          Some((name, dimension)) => (name, dimension.trim_end_matches('}')),
          None => (token, ""),
        };
        tokens.push((name.to_owned(), dimension.to_owned()));
      }
      rest = &after[end + 1..];
    }
    (count_word, tokens)
  }

  /// The English count words the doc-truth test understands (a roster is short).
  fn count_of(word: &str) -> Option<usize> {
    [
      "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
      "twelve",
    ]
    .iter()
    .position(|w| *w == word)
    .map(|index| index + 1)
  }

  /// The chokepoint-span roster is closed and is the design's (§4.14, GAP-A9-12): `ALL` is exactly the
  /// spans the design's own "*Span roster.*" sentence lists, in order, with their dimensions, and the
  /// count word the sentence opens with is the registry's count — so "nine chokepoints called seven"
  /// is a failing assertion against the design text, not a footnote. Do: parse the design's roster
  /// sentence. Expect: its count word, names and `{dimension}` labels equal the registry's.
  #[test]
  #[cfg_attr(
    miri,
    ignore = "reads the design document; the filesystem is unavailable under Miri's isolation"
  )]
  fn the_span_roster_is_the_designs_and_its_count_word_is_true() {
    let (count_word, listed) = design_roster();
    assert_eq!(
      count_of(&count_word),
      Some(Chokepoint::ALL.len()),
      "the design says \"{count_word} chokepoints\"; the registry holds {}",
      Chokepoint::ALL.len()
    );
    let registry: Vec<(String, String)> = Chokepoint::ALL
      .iter()
      .map(|point| (point.name().to_owned(), point.dimension().to_owned()))
      .collect();
    assert_eq!(
      listed, registry,
      "the design's roster (left) and the registry (right) name the same spans and dimensions in order"
    );
    let mut unique: Vec<&str> = Chokepoint::ALL.iter().map(|p| p.name()).collect();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
      unique.len(),
      Chokepoint::ALL.len(),
      "the span names are unique"
    );
  }

  /// The generated block of the record between its markers.
  fn recorded_table() -> String {
    let record = std::fs::read_to_string(RECORD).expect("docs/wip/observability.md is readable");
    let start = record
      .find(TABLE_BEGIN)
      .expect("the record has the chokepoints:begin marker")
      + TABLE_BEGIN.len();
    let end = record[start..]
      .find(TABLE_END)
      .expect("the record has the chokepoints:end marker")
      + start;
    record[start..end].to_owned()
  }

  /// The registry table in `docs/wip/observability.md` is generated from the registry (doc-truth): the
  /// block between the markers equals `Chokepoint::registry_table()` byte for byte. Do: read the
  /// record. Expect: equality; a drift in either direction fails, and `--ignored
  /// regenerate_the_chokepoint_table` rewrites the block deliberately.
  #[test]
  #[cfg_attr(
    miri,
    ignore = "reads docs/wip/observability.md; the filesystem is unavailable under Miri's isolation"
  )]
  fn the_recorded_chokepoint_table_is_the_registry() {
    assert_eq!(
      recorded_table(),
      Chokepoint::registry_table(),
      "docs/wip/observability.md's chokepoint table drifted from the registry; run `cargo test -p slates-wire --lib -- --ignored regenerate_the_chokepoint_table`"
    );
  }

  /// The deliberate writer: rewrites the generated block of the record from the registry. Ignored, so a
  /// normal test run never mutates the tree; run with `--ignored` after a registry change.
  #[test]
  #[ignore = "rewrites docs/wip/observability.md from the registry; run deliberately with --ignored"]
  fn regenerate_the_chokepoint_table() {
    let record = std::fs::read_to_string(RECORD).expect("docs/wip/observability.md is readable");
    let start = record
      .find(TABLE_BEGIN)
      .expect("the record has the chokepoints:begin marker")
      + TABLE_BEGIN.len();
    let end = record[start..]
      .find(TABLE_END)
      .expect("the record has the chokepoints:end marker")
      + start;
    let rewritten = format!(
      "{}{}{}",
      &record[..start],
      Chokepoint::registry_table(),
      &record[end..]
    );
    // The design's `--ignored regenerate` writer rewriting a tracked document in the repository (CLAUDE
    // §4 "Doc-truth tests"); shipped code never reaches this call.
    #[allow(clippy::disallowed_methods)]
    std::fs::write(RECORD, rewritten).expect("the record is writable");
  }

  /// A chokepoint's `index` is its position in `ALL` (§4.14): the [`ChokepointRegistry`] keys its flags
  /// by `index` and an exported span names its chokepoint by it, so if a variant's discriminant and its
  /// `ALL` position ever disagreed, a registration would set the wrong flag and an export would name the
  /// wrong span. This pins them together, and `from_index` back, so reordering cannot silently corrupt.
  #[test]
  fn a_chokepoints_index_is_its_roster_position_and_round_trips() {
    for (position, point) in Chokepoint::ALL.iter().enumerate() {
      assert_eq!(
        point.index(),
        position,
        "{} indexes its ALL slot",
        point.name()
      );
      assert_eq!(Chokepoint::from_index(position), Some(*point));
    }
    assert_eq!(
      Chokepoint::from_index(Chokepoint::ALL.len()),
      None,
      "a code past the roster names nothing"
    );
  }

  /// Every chokepoint declares what its absence means, who produces it, who observes it and its
  /// horizon (§4.14 A-9 "every signal specifies …"), and today every one is event-driven so its absence
  /// is `Unknown`, never a look-alike healthy zero. Pinned so a new chokepoint must decide these.
  #[test]
  fn every_chokepoint_declares_its_absence_producer_observer_and_horizon() {
    for point in Chokepoint::ALL {
      assert_eq!(
        point.absence(),
        AbsenceIs::Unknown,
        "{} is event-driven: absence is not known, not degraded",
        point.name()
      );
      assert!(!point.producer().name().is_empty());
      assert!(!point.observer().name().is_empty());
      assert!(!point.horizon().name().is_empty());
    }
    assert_eq!(AbsenceIs::Unknown.name(), "unknown");
    assert_eq!(AbsenceIs::Degraded.name(), "degraded");
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

  /// A request's spans are one trace with the causation the design states (§4.14 three-id law): the
  /// ring read opens the trace as a root; the verb within it and the log append within that each share
  /// the request and the trace and name their cause. Do: open a root for a request, a child within it,
  /// and a grandchild within the child; end all three. Expect: one request id and one trace on all
  /// three, three distinct span ids, `Root` → `Span(root)` → `Span(child)` causes — and the trace is not
  /// the request word, so neither identity can stand in for the other.
  #[test]
  fn a_child_span_shares_its_causes_request_and_trace_and_names_it() {
    let mut tracer = Tracer::new(0xA11CE, 3);
    let request = RequestId {
      client: 7,
      sequence: 42,
    };
    let ring = tracer.open_root(request, Chokepoint::RingRequest, 10);
    let op = tracer.open_within(&ring.context(), Chokepoint::ShardOp, 20);
    let append = tracer.open_within(&op.context(), Chokepoint::LogAppend, 30);
    let spans: [Span; 3] = [append.end(0, 40), op.end(1, 50), ring.end(3, 60)];
    for span in &spans {
      assert_eq!(
        span.context().request(),
        request,
        "the replay identity rides every span"
      );
      assert_eq!(
        span.context().trace(),
        ring.context().trace(),
        "one trace connects the request's spans"
      );
      assert_ne!(
        span.context().trace().0,
        u128::from(request.word()),
        "the trace is its own identity, not the request word widened"
      );
    }
    assert_eq!(ring.context().cause(), Cause::Root);
    assert_eq!(op.context().cause(), Cause::Span(ring.context().span()));
    assert_eq!(append.context().cause(), Cause::Span(op.context().span()));
    let mut ids = vec![
      ring.context().span(),
      op.context().span(),
      append.context().span(),
    ];
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 3, "three distinct span ids");
    assert_eq!(spans[1].duration_ns(), 30);
  }

  /// Two shards never mint the same span or trace id, and a retry of one request opens a new trace
  /// (§4.9 "these have different lifetimes"): the request identity outlives any one trace. Do: open the
  /// same request as a root on two tracers and twice on one. Expect: four distinct traces and spans.
  #[test]
  fn ids_are_distinct_across_shards_and_a_retry_opens_a_new_trace() {
    let request = RequestId {
      client: 1,
      sequence: 1,
    };
    let mut shard_a = Tracer::new(1, 0);
    let mut shard_b = Tracer::new(1, 1);
    let first = shard_a.open_root(request, Chokepoint::RingRequest, 0);
    let retry = shard_a.open_root(request, Chokepoint::RingRequest, 0);
    let elsewhere = shard_b.open_root(request, Chokepoint::RingRequest, 0);
    let mut traces = vec![
      first.context().trace(),
      retry.context().trace(),
      elsewhere.context().trace(),
    ];
    traces.sort_unstable();
    traces.dedup();
    assert_eq!(
      traces.len(),
      3,
      "a retry and another shard each open their own trace"
    );
    let mut spans = vec![
      first.context().span(),
      retry.context().span(),
      elsewhere.context().span(),
    ];
    spans.sort_unstable();
    spans.dedup();
    assert_eq!(spans.len(), 3, "span ids never collide across shards");
  }

  /// A span whose cause was not carried across a boundary declares the link missing (§4.14 "bounded
  /// rings report … missing causal links explicitly"), and is not passed off as a root. Do: open an
  /// unlinked span. Expect: `Cause::Missing`, its own trace, the request it serves.
  #[test]
  fn an_unlinked_span_declares_its_missing_cause() {
    let mut tracer = Tracer::new(9, 0);
    let request = RequestId {
      client: 3,
      sequence: 5,
    };
    let orphan = tracer.open_unlinked(request, Chokepoint::ShardOp, 1);
    assert_eq!(orphan.context().cause(), Cause::Missing);
    assert_eq!(orphan.context().request(), request);
    assert_ne!(orphan.context().cause(), Cause::Root, "missing is not root");
  }

  /// Shape: the sink bound the sink tests use — three, so five emitted spans shed exactly two.
  const SINK_CAPACITY: usize = 3;

  /// A sink of [`SINK_CAPACITY`] with five spans emitted into it (labels 0..5), for the sink tests.
  fn overfilled_sink(tracer: &mut Tracer) -> SpanSink {
    let mut sink = SpanSink::with_capacity(SINK_CAPACITY);
    assert!(sink.is_empty());
    for label in 0..5u32 {
      sink.emit(span_labeled(tracer, label));
    }
    sink
  }

  /// The telemetry sink is bounded, keeps the most recent spans (a ring), and counts every shed span
  /// explicitly (§4.14 "bounded rings report dropped spans"). Do: emit five spans into a sink of three.
  /// Expect: it holds the three most recent (oldest first) and counts the two it shed; a zero-capacity
  /// sink sheds and counts everything. The drop counter is the non-vacuity witness — a silently-lossless
  /// sink would fail.
  #[test]
  fn the_sink_is_bounded_keeps_the_most_recent_and_counts_every_shed_span() {
    let mut tracer = Tracer::new(1, 0);
    let sink = overfilled_sink(&mut tracer);
    assert_eq!(
      sink.len(),
      SINK_CAPACITY,
      "the sink never grows past its bound"
    );
    assert_eq!(
      sink.dropped(),
      2,
      "the two oldest spans were shed and counted"
    );
    let held: Vec<u32> = sink.spans().map(Span::label).collect();
    assert_eq!(
      held,
      vec![2, 3, 4],
      "the three most recent survived, oldest first"
    );
    // A zero-capacity sink sheds and counts everything it is given.
    let mut none = SpanSink::with_capacity(0);
    none.emit(span_labeled(&mut tracer, 9));
    assert_eq!(none.dropped(), 1);
    assert!(none.is_empty());
  }

  /// Each drain of the sink carries the loss before it as its marker, exactly once, and says what the
  /// bound left behind; the lifetime loss count is never reset by reading (§4.14 "bounded rings report
  /// dropped spans"). Do: overfill a sink of three (two shed), drain two, then the rest. Expect: the
  /// first batch marks `shed_before` 2 and `remaining` 1; the second marks 0 and 0; `dropped()` stays 2.
  #[test]
  fn each_drain_marks_the_loss_before_it_once_and_what_remains() {
    let mut tracer = Tracer::new(1, 0);
    let mut sink = overfilled_sink(&mut tracer);
    // A bounded drain: two of the three, the loss marker for the batch, one left behind.
    let drained = sink.drain_up_to(2);
    assert_eq!(
      drained.spans.iter().map(Span::label).collect::<Vec<_>>(),
      vec![2, 3]
    );
    assert_eq!(
      drained.shed_before, 2,
      "the loss before this batch is marked on it"
    );
    assert_eq!(
      drained.remaining, 1,
      "the bound left one span for the next drain"
    );
    // The next drain carries no loss (none happened between), and the lifetime count is kept.
    let rest = sink.drain_up_to(SINK_CAPACITY);
    assert_eq!(rest.spans.len(), 1);
    assert_eq!(
      rest.shed_before, 0,
      "a batch reports its own loss exactly once"
    );
    assert_eq!(rest.remaining, 0);
    assert!(sink.is_empty(), "the sink is empty after a full drain");
    assert_eq!(sink.dropped(), 2, "draining does not reset the loss signal");
  }

  /// A span measures its own duration and never underflows on a backwards clock (§4.14): end − start,
  /// saturating to zero. Do: a forward span, then one whose end precedes its start. Expect: the real
  /// duration, then zero (not a wrapped huge value).
  #[test]
  fn a_span_measures_its_own_duration_and_saturates_a_backwards_clock() {
    let mut tracer = Tracer::new(1, 0);
    let request = RequestId {
      client: 1,
      sequence: 1,
    };
    let forward = tracer
      .open_root(request, Chokepoint::ShardOp, 100)
      .end(7, 250);
    assert_eq!(forward.duration_ns(), 150);
    let backwards = tracer
      .open_root(request, Chokepoint::ShardOp, 250)
      .end(7, 100);
    assert_eq!(
      backwards.duration_ns(),
      0,
      "a backwards clock saturates to zero"
    );
  }

  /// A span with the given dimension code; the ids are whatever the tracer mints because the sink tests
  /// are about ordering and bounds, not identity.
  fn span_labeled(tracer: &mut Tracer, label: u32) -> Span {
    let request = RequestId {
      client: 1,
      sequence: 1,
    };
    tracer
      .open_root(request, Chokepoint::ShardOp, 0)
      .end(label, 0)
  }
}
