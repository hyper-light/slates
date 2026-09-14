//! The telemetry emission and export path (§4.14, D-23; GAP-A9-12): every chokepoint span a shard
//! emits lands in that shard's bounded shed-first ring ([`emit`]), and the operator reads the rings
//! through the `Telemetry` verb ([`drain_verb`]) — the CLI's `slates status` and the MCP `slates.status`
//! drain every shard's ring by it, so the same closed registry the daemon emits against is what an
//! operator sees, one definition on both surfaces (§4.12 parity).
//!
//! The export is bounded end to end. A reply rides one bulk chunk of the client's region, so a drain
//! takes at most [`spans_per_reply`] spans — derived at boot from the chunk's bytes and the encoded
//! sizes of the report's fixed part and one span, never a magic count (R3) — and reports how many it
//! left in the ring. Loss is never silent: the ring counts every span it sheds, and each batch carries
//! the loss before it (`shed_before`), the loss since boot, and the spans whose cause was not carried
//! across a boundary (`missing_links`) — the design's "bounded rings report dropped spans and missing
//! causal links explicitly".
//!
//! Freshness is typed, not implied ([`summarize`]): for every chokepoint in the registry the batch says
//! how many spans it holds and the newest one's age, and judges that age against the horizon — the
//! operator's failover SLO, the one horizon declared for "is this still alive?" — so a chokepoint whose
//! newest span is older than the horizon is reported *absent* with the registry's word for what that
//! means (`unknown`: not known to be live), never as a live value with a stale age hiding behind it. A
//! chokepoint with no span at all is absent the same way, and `expected` says whether any producer of
//! it runs on this host, so "nothing here can produce it" (a laptop's `ship.record`) reads differently
//! from "idle" (AC-0.11 "drop a producer … expect typed unknown/degraded freshness").
//!
//! Evidence: the chunk bound is `slates-ipc`'s `pack` (a body larger than the chunk is refused
//! `PayloadTooLarge`), measured by the unit test below; the per-shard ring is thread-local (R2, D-7).

use slates_ipc::protocol::{
  CauseRecord, ChokepointReport, Refusal, ReplyBody, SpanRecord, TelemetryReport, encode_body,
};
use slates_machine::{Derived, derived};
use slates_vfs::clock::Clock;
use slates_wire::Wire;
use slates_wire::observe::{Cause, Chokepoint, Drained, Producer, Span};

use crate::state::ShardState;
use crate::verbs::refused;

/// Emits one completed span into this shard's bounded ring (§4.14): a push, no await, no lock, so it
/// never blocks the work it measured; a full ring sheds its oldest span and counts the loss.
pub(crate) fn emit(state: &mut ShardState, span: Span) {
  state.telemetry.emit(span);
}

/// The `Telemetry` verb on the shard it names (§4.14): drains up to the reply quota of the ring, marks
/// the batch with the loss before it, and judges every chokepoint's freshness against the horizon. The
/// window a batch covers is the time since the previous drain (since boot for the first), so a reader
/// draining on a cadence sees exactly the spans of each interval, each once.
pub fn drain_verb(state: &mut ShardState, partition: u16) -> ReplyBody {
  if partition != state.partition {
    // `serve` routes a drain to the shard it names; reaching another shard is a routing bug, refused
    // typed rather than the wrong ring drained.
    return refused(Refusal::NotFound);
  }
  let now_ns = state.clock.monotonic_ns();
  let drained = state.telemetry.drain_up_to(state.telemetry_quota);
  let window_ns = now_ns.saturating_sub(state.last_drain_ns);
  state.last_drain_ns = now_ns;
  let horizon_ns = state.config.failover_slo_ns;
  let dropped_total = state.telemetry.dropped();
  let report = summarize(
    partition,
    now_ns,
    window_ns,
    horizon_ns,
    &drained,
    dropped_total,
    |point| expected_here(state, point),
  );
  ReplyBody::Telemetry { report }
}

/// Whether a producer of `point` runs on this host (§4.14 "expected producer"): a client verb always;
/// the bridge when the NFS transport is serving; a landing where the host can write; replication only in
/// a fleet with `f ≥ 1` (a laptop ships nothing); the configuration group in any fleet; the archive codec
/// nowhere yet (its codec pass is owed, §4.10).
fn expected_here(state: &ShardState, point: Chokepoint) -> bool {
  match point.producer() {
    Producer::ClientVerb => true,
    Producer::Bridge => crate::daemon::NFS_PORT.load(std::sync::atomic::Ordering::Acquire) != 0,
    Producer::Landing => cfg!(unix),
    Producer::Replication => state
      .config
      .fleet
      .as_ref()
      .is_some_and(|membership| membership.quorum.f >= 1),
    Producer::Consensus => state.config.fleet.is_some(),
    Producer::Archive => false,
  }
}

/// One batch's report (§4.14), pure: the registry in roster order with each chokepoint's count, its
/// newest span's age at `now_ns` and whether that age is within `horizon_ns` (fresh) — a chokepoint
/// with no span, or one whose newest is older than the horizon, is not fresh and carries the registry's
/// absence word — plus the spans themselves, the batch's loss markers and its missing causal links.
pub fn summarize(
  partition: u16,
  now_ns: u64,
  window_ns: u64,
  horizon_ns: u64,
  drained: &Drained,
  dropped_total: u64,
  expected: impl Fn(Chokepoint) -> bool,
) -> TelemetryReport {
  let mut counts = [0u64; Chokepoint::ALL.len()];
  let mut newest_end = [None::<u64>; Chokepoint::ALL.len()];
  let mut missing_links = 0u64;
  let spans: Vec<SpanRecord> = drained
    .spans
    .iter()
    .map(|span| {
      let index = span.point().index();
      counts[index] = counts[index].saturating_add(1);
      newest_end[index] =
        Some(newest_end[index].map_or(span.end_ns(), |end| end.max(span.end_ns())));
      if span.context().cause() == Cause::Missing {
        missing_links = missing_links.saturating_add(1);
      }
      record_of(span)
    })
    .collect();
  let chokepoints = Chokepoint::ALL
    .iter()
    .map(|point| {
      let latest_age_ns = newest_end[point.index()].map(|end| now_ns.saturating_sub(end));
      ChokepointReport {
        name: point.name().to_owned(),
        dimension: point.dimension().to_owned(),
        spans: counts[point.index()],
        latest_age_ns,
        fresh: latest_age_ns.is_some_and(|age| age <= horizon_ns),
        absence: point.absence(),
        producer: point.producer().name().to_owned(),
        expected: expected(*point),
      }
    })
    .collect();
  TelemetryReport {
    partition,
    now_ns,
    window_ns,
    horizon_ns,
    shed_before: drained.shed_before,
    dropped_total,
    remaining: u64::try_from(drained.remaining).unwrap_or(u64::MAX),
    missing_links,
    chokepoints,
    spans,
  }
}

/// A span's wire record: the three identities distinct on the wire as they are in the type — the
/// request as the client's `(client, sequence)`, the 128-bit trace as two words, the span, its cause.
fn record_of(span: &Span) -> SpanRecord {
  let context = span.context();
  let trace = context.trace().0;
  SpanRecord {
    point: u32::try_from(span.point().index()).unwrap_or(u32::MAX),
    label: span.label(),
    request_client: context.request().client,
    request_sequence: context.request().sequence,
    trace_high: u64::try_from(trace >> u64::BITS).unwrap_or(u64::MAX),
    trace_low: u64::try_from(trace & u128::from(u64::MAX)).unwrap_or(u64::MAX),
    span: context.span().0,
    cause: match context.cause() {
      Cause::Root => CauseRecord::Root,
      Cause::Span(id) => CauseRecord::Span { id: id.0 },
      Cause::Missing => CauseRecord::Missing,
    },
    start_ns: span.start_ns(),
    end_ns: span.end_ns(),
  }
}

/// How many spans one `Telemetry` reply may carry (§4.14, R3): the reply rides one bulk chunk of the
/// client's region (`chunk_bytes`), so the quota is the chunk less the encoded size of the report's
/// fixed part (the nine registry entries at their widest, every counter, the schema word) divided by
/// the encoded size of one span record at its widest. Measured from the wire encoding itself, at boot,
/// so a change to either type re-derives it; zero is the honest degenerate when a chunk cannot hold
/// even the fixed part (the drain then reports counts and freshness and leaves every span in the ring).
pub fn spans_per_reply(chunk_bytes: usize) -> Derived<usize> {
  let fixed = fixed_report_bytes();
  let per_span = span_record_bytes().max(1);
  derived!(
    chunk_bytes.saturating_sub(fixed) / per_span,
    "(bulk chunk bytes − encoded fixed report bytes) / encoded span record bytes",
    [
      "ipc.bulk_chunk_bytes",
      "wire.TelemetryReport",
      "wire.SpanRecord"
    ]
  )
}

/// The encoded bytes of a `Telemetry` reply with every registry entry at its widest and no spans.
fn fixed_report_bytes() -> usize {
  let widest = TelemetryReport {
    partition: u16::MAX,
    now_ns: u64::MAX,
    window_ns: u64::MAX,
    horizon_ns: u64::MAX,
    shed_before: u64::MAX,
    dropped_total: u64::MAX,
    remaining: u64::MAX,
    missing_links: u64::MAX,
    chokepoints: Chokepoint::ALL
      .iter()
      .map(|point| ChokepointReport {
        name: point.name().to_owned(),
        dimension: point.dimension().to_owned(),
        spans: u64::MAX,
        latest_age_ns: Some(u64::MAX),
        fresh: true,
        absence: point.absence(),
        producer: point.producer().name().to_owned(),
        expected: true,
      })
      .collect(),
    spans: Vec::new(),
  };
  encode_body(&ReplyBody::Telemetry { report: widest }).len()
}

/// The encoded bytes of one span record at its widest (a `Span` cause carries an id; the others none).
fn span_record_bytes() -> usize {
  let widest = SpanRecord {
    point: u32::MAX,
    label: u32::MAX,
    request_client: u32::MAX,
    request_sequence: u32::MAX,
    trace_high: u64::MAX,
    trace_low: u64::MAX,
    span: u64::MAX,
    cause: CauseRecord::Span { id: u64::MAX },
    start_ns: u64::MAX,
    end_ns: u64::MAX,
  };
  let mut out = Vec::new();
  widest.encode(&mut out);
  out.len()
}

#[cfg(test)]
mod tests {
  use super::{fixed_report_bytes, span_record_bytes, spans_per_reply, summarize};
  use slates_ipc::protocol::{
    AbsenceIs, CauseRecord, ChokepointReport, ReplyBody, TelemetryReport, encode_body,
  };
  use slates_wire::observe::{Chokepoint, Drained, SpanSink, Tracer};
  use slates_wire::request::RequestId;

  /// Shape: the bulk chunk a reply rides in the tests, the daemon's own `BULK_CHUNK_BYTES` (one page).
  const CHUNK: usize = 4096;
  /// Shape: a freshness horizon for the pure summarizer: one second, so ages of 1 ms and 2 s fall on
  /// either side of it.
  const HORIZON_NS: u64 = 1_000_000_000;
  /// Shape: "now" for the pure summarizer, later than every span the tests build.
  const NOW_NS: u64 = 10_000_000_000;

  /// The request the sample batch serves.
  const REQUEST: RequestId = RequestId {
    client: 4,
    sequence: 9,
  };

  /// A summarized sample batch: a fresh `shard.op` (1 ms old) and a stale `log.append` (2 s old), both
  /// caused by one ring span, and an unlinked `merge.verdict`; two spans shed before it, one left behind;
  /// every chokepoint but `archive.chunk` expected. Returns the report and the ring span's id.
  fn sample_batch() -> (TelemetryReport, u64) {
    let mut tracer = Tracer::new(1, 0);
    let ring = tracer.open_root(REQUEST, Chokepoint::RingRequest, 0);
    let fresh_op = tracer
      .open_within(&ring.context(), Chokepoint::ShardOp, NOW_NS - 2_000_000)
      .end(1, NOW_NS - 1_000_000);
    let stale_append = tracer
      .open_within(
        &ring.context(),
        Chokepoint::LogAppend,
        NOW_NS - 3_000_000_000,
      )
      .end(0, NOW_NS - 2_000_000_000);
    let unlinked = tracer
      .open_unlinked(REQUEST, Chokepoint::MergeVerdict, NOW_NS - 10)
      .end(1, NOW_NS - 5);
    let drained = Drained {
      spans: vec![fresh_op, stale_append, unlinked],
      shed_before: 2,
      remaining: 1,
    };
    let report = summarize(3, NOW_NS, 500, HORIZON_NS, &drained, 7, |point| {
      point != Chokepoint::ArchiveChunk
    });
    (report, ring.context().span().0)
  }

  /// The report's entry for a chokepoint, by name.
  fn entry_of<'a>(report: &'a TelemetryReport, name: &str) -> &'a ChokepointReport {
    report
      .chokepoints
      .iter()
      .find(|c| c.name == name)
      .unwrap_or_else(|| panic!("{name} is in the report"))
  }

  /// A chokepoint whose newest span is within the horizon is fresh and reports its age, and the batch
  /// carries the whole registry in roster order (§4.14). Do: summarize the sample batch. Expect: nine
  /// entries in roster order; `shard.op` fresh with its 1 ms age, one span, expected here.
  #[test]
  fn a_fresh_chokepoint_reports_its_age() {
    let (report, _) = sample_batch();
    let names: Vec<&str> = report.chokepoints.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
      names,
      Chokepoint::ALL.iter().map(|p| p.name()).collect::<Vec<_>>(),
      "the registry, in roster order"
    );
    let op = entry_of(&report, "shard.op");
    assert!(op.fresh, "1 ms old is within a 1 s horizon");
    assert_eq!(op.latest_age_ns, Some(1_000_000));
    assert_eq!(op.spans, 1);
    assert!(op.expected);
  }

  /// A chokepoint that has not reported within the horizon is typed absent, not a stale value; one with
  /// no span at all is absent with no age (§4.14, AC-0.11 "drop a producer"). Do: summarize the sample
  /// batch. Expect: `log.append` not fresh yet carrying its 2 s last-seen age and the registry's
  /// `unknown`; `archive.chunk` absent with no age and `expected` false.
  #[test]
  fn a_chokepoint_past_its_horizon_is_typed_absent_not_a_stale_value() {
    let (report, _) = sample_batch();
    let append = entry_of(&report, "log.append");
    assert!(!append.fresh, "2 s old is past the horizon: typed absent");
    assert_eq!(
      append.latest_age_ns,
      Some(2_000_000_000),
      "the last sighting is still stated, as an age, never as a live value"
    );
    assert_eq!(append.absence, AbsenceIs::Unknown);
    let archive = entry_of(&report, "archive.chunk");
    assert!(!archive.fresh);
    assert_eq!(archive.latest_age_ns, None, "nothing ever reported it");
    assert!(!archive.expected, "no producer of it runs on this host");
  }

  /// A batch carries its loss markers and its missing causal links explicitly (§4.14 "bounded rings
  /// report dropped spans and missing causal links"). Do: summarize the sample batch. Expect:
  /// `shed_before` 2, `dropped_total` 7, `remaining` 1, `missing_links` 1, three spans, the window and
  /// partition as given.
  #[test]
  fn a_batch_marks_its_loss_and_its_missing_links() {
    let (report, _) = sample_batch();
    assert_eq!(report.partition, 3);
    assert_eq!(
      report.shed_before, 2,
      "the loss before the batch is its marker"
    );
    assert_eq!(report.dropped_total, 7);
    assert_eq!(report.remaining, 1);
    assert_eq!(report.missing_links, 1, "the unlinked span is reported");
    assert_eq!(report.spans.len(), 3);
    assert_eq!(report.window_ns, 500);
  }

  /// Every span record keeps the three identities distinct on the wire (§4.14 three-id law). Do:
  /// summarize the sample batch. Expect: the `shard.op` record names the request, is caused by the ring
  /// span, and its trace is not the request word; the unlinked span's cause is `Missing`.
  #[test]
  fn a_span_record_keeps_the_three_identities_distinct() {
    let (report, ring_span) = sample_batch();
    let op_record = &report.spans[0];
    assert_eq!(
      (op_record.request_client, op_record.request_sequence),
      (REQUEST.client, REQUEST.sequence)
    );
    assert_eq!(op_record.cause, CauseRecord::Span { id: ring_span });
    assert_eq!(report.spans[2].cause, CauseRecord::Missing);
    assert_ne!(
      (op_record.trace_high, op_record.trace_low),
      (0, REQUEST.word()),
      "the trace is not the request word"
    );
  }

  /// The reply quota is what one chunk holds, measured from the encoding (R3): a report carrying
  /// exactly the quota of spans at their widest fits the chunk, one more does not, and a chunk too small
  /// for even the fixed part yields the honest zero. Do: derive the quota for the daemon's chunk, build
  /// reports at the quota and past it, encode them. Expect: ≤ chunk, > chunk, and 0 for a tiny chunk.
  /// The measured sizes are printed so the record can quote them.
  #[test]
  fn the_reply_quota_is_exactly_what_one_chunk_holds() {
    let fixed = fixed_report_bytes();
    let per_span = span_record_bytes();
    let quota = spans_per_reply(CHUNK);
    println!(
      "telemetry reply: fixed part {fixed} bytes, span record {per_span} bytes, quota {} spans per {CHUNK}-byte chunk ({})",
      quota.get(),
      quota.formula
    );
    assert!(quota.get() > 0, "a page holds spans");
    let mut tracer = Tracer::new(u64::MAX, u16::MAX);
    let request = RequestId {
      client: u32::MAX,
      sequence: u32::MAX,
    };
    let mut sink = SpanSink::with_capacity(quota.get() + 1);
    let root = tracer.open_root(request, Chokepoint::RingRequest, u64::MAX);
    for _ in 0..=quota.get() {
      sink.emit(
        tracer
          .open_within(&root.context(), Chokepoint::ShardOp, u64::MAX)
          .end(u32::MAX, u64::MAX),
      );
    }
    let widest = |drained: &Drained| {
      let mut report: TelemetryReport = summarize(
        u16::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        drained,
        u64::MAX,
        |_| true,
      );
      for entry in &mut report.chokepoints {
        entry.spans = u64::MAX;
        entry.latest_age_ns = Some(u64::MAX);
      }
      encode_body(&ReplyBody::Telemetry { report }).len()
    };
    let at_quota = sink.drain_up_to(quota.get());
    assert!(widest(&at_quota) <= CHUNK, "the quota fits the chunk");
    let one_more = Drained {
      spans: {
        let mut spans = at_quota.spans.clone();
        spans.extend(sink.drain_up_to(1).spans);
        spans
      },
      shed_before: u64::MAX,
      remaining: usize::MAX,
    };
    assert_eq!(one_more.spans.len(), quota.get() + 1);
    assert!(widest(&one_more) > CHUNK, "one past the quota does not fit");
    assert_eq!(
      spans_per_reply(fixed - 1).get(),
      0,
      "too small a chunk: no spans, honestly"
    );
  }
}
