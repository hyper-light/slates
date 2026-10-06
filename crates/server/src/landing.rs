//! Landings and grants through the server (§4.15, §4.13, D-26; Phase 2 task 8). A client asks
//! to land a snapshot's diverged entries onto a host directory (`RequestBody::Land`, over the
//! ring); the server plans the manifest and, without a grant, replies `GrantRequired` with the
//! manifest, its summary and the conflicts a preliminary pass found, and records the plan in
//! the durable audit log. A human then issues a grant through `slates grant` — never on the
//! ring or MCP (R10, AC-2.8): the grant travels on the control channel, binds the manifest
//! hash, and is persisted as a `GrantRecord`. The client lands again with the grant; the
//! landing takes the target's lease (one holder per target, AC-2.9), validates, writes through
//! `slates-land`'s engine over the real writer (`OsLand`), and the outcome, the lease and every
//! audit record are persisted so the accountability survives a crash (AC-2.10).
//!
//! The engine and the manifest are `slates-land`'s (Phase 1); this module is the wiring: it
//! holds the runtime grant, lease and audit structures per shard, mirrors their mutations into
//! the database's durable records, and maps the engine's refusals to the wire taxonomy. The
//! write path runs on Linux and macOS; its tests land into a real directory on the host's disk in
//! the build output (`CARGO_TARGET_TMPDIR`; A-50: never `/tmp`, never a RAM directory), on every
//! host, exactly as the Phase 1 landing tests do.

use slates_db::Op;
#[cfg(unix)]
use slates_db::catalog::LandingRecord;
use slates_db::catalog::{
  AuditKind as DbAuditKind, AuditRecord as DbAuditRecord, GrantRecord as DbGrantRecord,
  GrantScope as DbGrantScope, GrantState as DbGrantState, GrantSurface,
  LandingState as DbLandingState, Principal, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId,
};
#[cfg(unix)]
use slates_ipc::protocol::{
  ActionCount, HostAnswer, LandingDegradation, LandingDurability, LandingOutcome, LandingSummary,
};
use slates_ipc::protocol::{
  AuditEntry, GrantScope, GrantSummary, Refusal, ReplyBody, SnapshotId, VolumeId,
};
use slates_land::engine::Audit;
#[cfg(unix)]
use slates_land::engine::{AuditKind, AuditRecord};
#[cfg(unix)]
use slates_land::engine::{
  Degradation, Durability, HostSlot, LandingRefusal, LandingReport, LandingRequest, LandingRun,
  Observer, begin_landing, land, settle_grant,
};
use slates_land::grant::{
  GrantBinding, GrantRecord as LandGrantRecord, GrantScope as LandScope,
  GrantState as LandGrantState, Grants, Surface, TargetIdentity,
};
#[cfg(unix)]
use slates_land::grant::{GrantId, GrantRefusal, LandingLease};
#[cfg(unix)]
use slates_land::manifest::{Filter, LandingEntry, Manifest};
#[cfg(unix)]
use slates_land::os::{Away, OsLand, TargetRefusal};
#[cfg(unix)]
use slates_land::source::Source;
use slates_vfs::clock::Clock;
#[cfg(unix)]
use slates_vfs::host::{HostError, HostFs, LandFs};
#[cfg(unix)]
use slates_wire::observe::Chokepoint;

#[cfg(unix)]
use crate::error::refusal_of_vfs;
use crate::state::ShardState;
use crate::verbs::refused;
#[cfg(unix)]
use crate::verbs::{find, forbidden, rights_of, to_db_snapshot, to_db_volume};

/// Shape: how many audit records the server keeps in memory before the ring trims them; the
/// durable record is the database's, and this only bounds the in-process mirror (§4.15's
/// derived retention is wired with the landing-rate measurement in a later phase).
const AUDIT_RETAIN: usize = 4096;

/// The runtime landing state a shard holds (§4.15): the grants and the audit log, each mirrored into the
/// durable database as it changes; the presentations awaiting a grant; and the granted landings running as
/// owned tasks. Target leases are not kept here: they are durable records on the control shard, one owner
/// for the host's targets (AUD-29-03; before 2026-09-29 each shard kept its own table).
pub struct LandingState {
  /// The runtime grants (mirrored to `GrantRecord`).
  pub grants: Grants,
  /// The runtime audit log (mirrored to `AuditRecord`).
  pub audit: Audit,
  /// The next landing id.
  pub next_landing: u64,
  /// A landing awaiting a grant: its id, the manifest it planned, the volume and the target,
  /// so a grant on the control channel binds the right manifest (§4.15 step 3).
  pub awaiting: std::collections::BTreeMap<u64, Awaiting>,
  /// Granted landings running as owned tasks on this shard, by the request that started each (its origin,
  /// client and sequence), with where its reply goes: the newest attempt's route, since a retry comes from
  /// where its client now waits. A retry joins the landing and never starts a second. One entry per task,
  /// so bounded by the shard's task arena.
  pub in_flight:
    std::collections::BTreeMap<(u64, u32, u32), Option<crate::merge_service::ReplyRoute>>,
  /// The next landing attempt this shard numbers: the holder a target lease is taken for, one per attempt,
  /// so two landings never share a lease — not even two of one principal.
  pub next_holder: u64,
  /// The volumes a granted landing is running on, with its attempt (AUD-29-25): a landing runs in slices
  /// with the shard serving between them, so a second landing of the same volume is refused while one runs —
  /// both would advance one overlay. One entry per running landing, so bounded by the shard's task arena.
  pub running: std::collections::BTreeMap<DbVolumeId, u64>,
  /// How long each slice of a granted landing on this shard took, in nanoseconds (AUD-29-25's percentile
  /// lane: a landing is stepped in slices of its budget, the shard serving between them).
  pub slice_times: crate::histogram::DurationHistogram,
  /// The slices that ran past their budget by more than their own last unit — a step that kept going after
  /// its budget had passed. Zero is the evidence that every slice ended within one unit of its budget.
  pub slices_past_budget: u64,
  /// The budget the last slice was given, in nanoseconds (half the shard's step quantum then).
  pub slice_budget_ns: u64,
}

/// What a shard's granted-landing slices measured (AUD-29-25), for a run to publish.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SliceSummary {
  /// Slices taken.
  pub slices: u64,
  /// The median slice, in nanoseconds (a bucket's upper bound, within an eighth above the exact value).
  pub p50_ns: u64,
  /// The 99th percentile slice, likewise.
  pub p99_ns: u64,
  /// The 99.9th percentile slice, likewise.
  pub p999_ns: u64,
  /// The longest slice, exactly.
  pub max_ns: u64,
  /// Slices past their budget by more than their last unit.
  pub past_budget: u64,
  /// The budget the last slice was given.
  pub budget_ns: u64,
}

impl LandingState {
  /// The slices' summary.
  pub fn slice_summary(&self) -> SliceSummary {
    /// Format: the published quantiles, in parts per million.
    const P50: u64 = crate::histogram::PPM / 2;
    /// Format: see [`P50`].
    const P99: u64 = 990_000;
    /// Format: see [`P50`].
    const P999: u64 = 999_000;
    SliceSummary {
      slices: self.slice_times.count(),
      p50_ns: self.slice_times.quantile(P50),
      p99_ns: self.slice_times.quantile(P99),
      p999_ns: self.slice_times.quantile(P999),
      max_ns: self.slice_times.max(),
      past_budget: self.slices_past_budget,
      budget_ns: self.slice_budget_ns,
    }
  }
}

/// A landing presented and waiting for a grant.
#[derive(Clone, Debug)]
pub struct Awaiting {
  /// The manifest hash the grant must bind.
  pub manifest: [u8; 32],
  /// Who lands what where, as presented: the consumer, the volume and snapshot, and the target's identity
  /// when it was opened. The grant binds exactly this (§4.13 "Grants"; AUD-29-01).
  pub binding: slates_land::grant::GrantBinding,
  /// The volume.
  pub volume: DbVolumeId,
  /// The snapshot.
  pub snapshot: DbSnapshotId,
  /// The target.
  pub target: String,
  /// The principal it was presented to.
  pub principal: Principal,
  /// The client that presented it: its retirement abandons the presentation (AUD-29-07).
  pub client: u32,
}

impl std::fmt::Debug for LandingState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("LandingState")
      .field("next_landing", &self.next_landing)
      .field("awaiting", &self.awaiting.len())
      .finish()
  }
}

impl Default for LandingState {
  fn default() -> LandingState {
    LandingState {
      grants: Grants::default(),
      audit: Audit::new(AUDIT_RETAIN),
      next_landing: 1,
      awaiting: std::collections::BTreeMap::new(),
      in_flight: std::collections::BTreeMap::new(),
      next_holder: 1,
      running: std::collections::BTreeMap::new(),
      slice_times: crate::histogram::DurationHistogram::default(),
      slices_past_budget: 0,
      slice_budget_ns: 0,
    }
  }
}

/// The identity of a landing: its id and what it lands where.
#[cfg(unix)]
struct LandingIds<'a> {
  landing_id: u64,
  /// Whether `landing_id` was allocated for this call (a presentation, or a granted landing no presentation
  /// made) rather than taken from the presentation the grant was issued from.
  fresh: bool,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: &'a str,
  /// Whether this landing made a scratch volume an overlay over `target` (§4.15 step 9): its record then
  /// names the new base, so a recovery reacquires it.
  rebased: bool,
}

/// What a `land` request names (§4.15).
pub struct LandCall<'a> {
  /// The volume.
  pub volume: VolumeId,
  /// The snapshot, or the head.
  pub snapshot: Option<SnapshotId>,
  /// The target directory's path.
  pub target: &'a str,
  /// The filter.
  pub filter: &'a slates_ipc::protocol::Filter,
  /// The grant, when one was issued.
  pub grant: Option<u64>,
}

/// Lands `volume`'s snapshot into `target` (§4.15). Without a grant covering the planned
/// manifest, the reply is `GrantRequired`; with one, the landing runs and the reply is
/// `Landed`. Conflicts, a held lease and a mismatched grant are typed refusals. `client_id` is the
/// calling client, whose presentations its retirement abandons (AUD-29-07).
pub fn land_verb(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  call: LandCall<'_>,
) -> ReplyBody {
  // A merge volume has no store-backed snapshot to land (§4.16; the extent-backed green is owed).
  if let Some(reply) = crate::merge_service::refuse_store_verb(
    state,
    call.volume,
    crate::merge_service::StoreVerb::Land,
  ) {
    return reply;
  }
  #[cfg(unix)]
  {
    land_verb_unix(state, client_id, principal, call)
  }
  #[cfg(not(unix))]
  {
    // The write path is the `os` module (Unix), the landing engine's only real writer; the
    // Windows writer arrives with the Windows bridge (Phase 4). The verb refuses cleanly here.
    let _ = (state, client_id, principal, call);
    refused(Refusal::Unsupported {
      feature: "landing".to_owned(),
    })
  }
}

/// Takes the landing lease on the target whose canonical key is `key`, on this shard — the control shard,
/// the one owner of the host's target leases (§4.15 "Ownership facts", step 4; AUD-29-03) — for the landing
/// attempt `holder`, for `term_ns`, as a durable record. Refused `LandingLeaseHeld`, naming the holder,
/// while any attempt's lease on that target is unexpired. Refused `LandingLeaseLost`, taking nothing, once
/// the caller's `deadline_ns` has passed: the caller no longer waits for the answer, and a lease it never
/// learned of would hold the target for a term. The generation is the take's log sequence on this
/// partition, so it only grows, across releases and restarts: a release or a fence naming an older one
/// names a lease that is gone.
#[cfg(unix)]
pub fn take_target_lease(
  state: &mut ShardState,
  key: String,
  holder: u64,
  term_ns: u64,
  deadline_ns: u64,
) -> Result<LandingLease, Refusal> {
  let now = state.clock.monotonic_ns();
  if now >= deadline_ns {
    state.count(LEASE_TAKE_LATE, 1);
    return Err(Refusal::LandingLeaseLost);
  }
  release_expired_leases(state, now)?;
  let generation = state.db.next_seq();
  let expires_ns = now.saturating_add(term_ns);
  let record = slates_db::catalog::LandingLeaseRecord {
    target: key.clone(),
    holder,
    generation,
    expires_ns,
  };
  match state
    .db
    .mutate(&mut state.segment, &Op::LandingLeaseTaken { record }, now)
  {
    Ok(_) => Ok(LandingLease {
      target: key.into_boxed_str(),
      holder,
      generation,
      expires_ns,
    }),
    Err(slates_db::DbError::LeaseHeld { .. }) => Err(Refusal::LandingLeaseHeld {
      holder: state
        .db
        .partition()
        .landing_lease(&key)
        .map_or(0, |held| held.holder),
    }),
    Err(e) => Err(crate::error::refusal_of_db(&e)),
  }
}

/// Format: the landing-plane counter of a target-lease take that reached the control shard after its
/// caller's deadline, and took nothing.
#[cfg(unix)]
const LEASE_TAKE_LATE: &str = "landing.lease_take_late";

/// Releases every target lease whose term has ended, so the control partition holds no more records than
/// the leases live at its last take (ban 8): a lease whose release never came — its attempt's compensation
/// lost as well, or its daemon gone — would otherwise stay for good. Bounded by those records.
#[cfg(unix)]
fn release_expired_leases(state: &mut ShardState, now: u64) -> Result<(), Refusal> {
  let expired: Vec<String> = state
    .db
    .partition()
    .landing_leases()
    .filter(|lease| lease.expires_ns <= now)
    .map(|lease| lease.target.clone())
    .collect();
  for target in expired {
    state
      .db
      .mutate(
        &mut state.segment,
        &Op::LandingLeaseReleased { target },
        now,
      )
      .map_err(|e| crate::error::refusal_of_db(&e))?;
  }
  Ok(())
}

/// Releases the target lease on `key` when the landing attempt `holder` still holds it, on this (the
/// control) shard, and nothing otherwise: an attempt whose term ended, and whose target another has taken
/// since, releases nothing of the new holder's. It is also the compensation for a take whose answer never
/// came back: the attempt's own lease, if that take landed.
#[cfg(unix)]
pub fn release_target_lease(state: &mut ShardState, key: &str, holder: u64) -> Result<(), Refusal> {
  let held = state
    .db
    .partition()
    .landing_lease(key)
    .is_some_and(|lease| lease.holder == holder);
  if !held {
    return Ok(());
  }
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(
      &mut state.segment,
      &Op::LandingLeaseReleased {
        target: key.to_owned(),
      },
      now,
    )
    .map(|_| ())
    .map_err(|e| crate::error::refusal_of_db(&e))
}

/// A retry of a granted landing still running joins it (AUD-29-03): the verb does not run again, and the
/// reply goes where this attempt came from when it came from somewhere. Whether the request was running.
pub(crate) fn join_in_flight(
  state: &mut ShardState,
  origin: u64,
  id: slates_wire::request::RequestId,
) -> bool {
  let Some(route) = state
    .landing
    .in_flight
    .get_mut(&(origin, id.client, id.sequence))
  else {
    return false;
  };
  if let Some(newest) = state.reply_route.take() {
    *route = Some(newest);
  }
  true
}

/// Drops every landing `client` presented on this shard and never landed: its retirement abandons them
/// (AUD-29-07), so a presentation does not outlive the client waiting on it. Bounded by the shard's
/// presentations.
pub fn abandon_presentations(state: &mut ShardState, client: u32) {
  state
    .landing
    .awaiting
    .retain(|_, awaiting| awaiting.client != client);
}

/// A landing observer (§4.14) that records one `land.entry` timing per entry into a bounded buffer, so
/// a large landing never grows it without bound (ban 8): at capacity it sheds the oldest timing and
/// counts the loss. It records only `(start, end)` — the land engine stays wire-free; the server builds
/// the spans and assigns the shard's ids when it drains the observer ([`drain_land_spans`]).
#[cfg(unix)]
struct SpanObserver {
  entries: std::collections::VecDeque<(u64, u64)>,
  capacity: usize,
  dropped: u64,
}

#[cfg(unix)]
impl SpanObserver {
  fn with_capacity(capacity: usize) -> SpanObserver {
    SpanObserver {
      entries: std::collections::VecDeque::with_capacity(capacity),
      capacity,
      dropped: 0,
    }
  }

  /// Records one entry's `(start, end)` timing, bounded shed-first: at capacity the oldest timing is
  /// shed and counted (or this one, when the bound is zero), so the buffer never grows past its bound.
  fn record(&mut self, start_ns: u64, end_ns: u64) {
    if self.entries.len() == self.capacity {
      if self.entries.pop_front().is_none() {
        self.dropped = self.dropped.saturating_add(1);
        return;
      }
      self.dropped = self.dropped.saturating_add(1);
    }
    self.entries.push_back((start_ns, end_ns));
  }
}

#[cfg(unix)]
impl<H: LandFs> Observer<H> for SpanObserver {
  fn before_write(&mut self, _host: &mut H, _entry: &LandingEntry) {}

  fn after_entry(&mut self, start_ns: u64, end_ns: u64) {
    self.record(start_ns, end_ns);
  }
}

/// Drains a landing's observed entry timings into the shard's telemetry ring as `land.entry` spans
/// (§4.14), each opened within the `shard.op` span of the verb that ran the landing — sharing its
/// request and trace and naming it as the cause — and folds the observer's shed count into the ring's
/// loss total. Called after `land` returns, so the ring is clear of the landing's borrows.
#[cfg(unix)]
fn drain_land_spans(state: &mut ShardState, observer: SpanObserver) {
  let cause = state.current_span;
  let dropped = observer.dropped;
  for (start_ns, end_ns) in observer.entries {
    let open = match cause {
      Some(cause) => state
        .tracer
        .open_within(&cause, Chokepoint::LandEntry, start_ns),
      // A landing always runs inside a recorded verb; without its span the cause is declared missing
      // rather than invented.
      None => state.tracer.open_unlinked(
        slates_wire::request::RequestId::default(),
        Chokepoint::LandEntry,
        start_ns,
      ),
    };
    crate::telemetry::emit(state, open.end(0, end_ns));
  }
  state.telemetry.record_dropped(dropped);
}

/// The Unix landing: open the target through `OsLand` and run the engine.
#[cfg(unix)]
fn land_verb_unix(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  call: LandCall<'_>,
) -> ReplyBody {
  let LandCall {
    volume,
    snapshot,
    target,
    filter,
    grant,
  } = call;
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("land");
  }
  let db_volume = to_db_volume(volume);
  // A landing without a grant ends in a presentation, and the shard holds only so many: past the bound it
  // is refused before any host access, id or record (AUD-29-07).
  if grant.is_none() && presentations_full(state, client_id, db_volume, target) {
    return refused(Refusal::LandingsAwaitingFull);
  }
  let db_snapshot = snapshot.map_or(record.head, to_db_snapshot);
  // A named snapshot is what lands, exactly as it froze; an unnamed landing lands the head. Resolved before
  // any host access (AUD-29-02, A-49).
  let source = match snapshot {
    Some(named) => match named_snapshot_source(state, handle, &record, named) {
      Ok(source) => source,
      Err(refusal) => return refused(refusal),
    },
    None => Source::Head,
  };
  // Open the target: this is the only place the server touches a host path for writing, and
  // only under the grant checked below (R1, R10). A path that cannot be opened, or that
  // escapes containment, is a typed refusal with no write.
  let (os, land_target) = match OsLand::open_target(std::path::Path::new(target)) {
    Ok(pair) => pair,
    Err(refusal) => return refused(target_refusal(&refusal)),
  };
  let prepared = Prepared {
    handle,
    os,
    land_target,
    filter: to_land_filter(filter),
    source,
    volume: db_volume,
    snapshot: db_snapshot,
    target: target.to_owned(),
    principal: principal.clone(),
    client: client_id,
    grant,
  };
  match grant {
    // A presentation writes nothing and takes no lease: it is answered now.
    None => run_landing(state, prepared, None),
    // A granted landing runs as an owned task under its target's lease (AUD-29-03).
    Some(_) => defer_granted_landing(state, prepared),
  }
}

/// A landing ready to run: its volume, its opened target and what its request and records name.
#[cfg(unix)]
struct Prepared {
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  os: OsLand,
  land_target: slates_land::engine::LandingTarget,
  filter: Filter,
  source: Source,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: String,
  principal: Principal,
  client: u32,
  grant: Option<u64>,
}

/// Runs the engine for `prepared` under `lease` (none for a presentation) and turns its outcome into the
/// reply: the finished landing's records, the presentation, or the typed refusal. The landing's id and clock
/// are read here, as it runs — for a granted landing after its wait for the lease, so a presentation another
/// landing consumed meanwhile is not landed twice under one id.
#[cfg(unix)]
fn run_landing(
  state: &mut ShardState,
  prepared: Prepared,
  lease: Option<&LandingLease>,
) -> ReplyBody {
  // The id names its owner partition (`verbs::landing_id`), so the `Grant` that later covers it routes
  // to this shard, where the presented record waits. A granted landing lands under the id of the
  // presentation its grant was issued from, so its finish consumes that presentation and a resume under the
  // same grant keeps the id its hidden siblings carry (AUD-29-07; before 2026-09-29 every call took a fresh
  // id, the presentation was never consumed, and a resume could not find its crashed attempt's siblings).
  let presented = prepared.grant.and_then(|g| presentation_for(state, g));
  let landing_id = presented
    .unwrap_or_else(|| crate::verbs::landing_id(state.partition, state.landing.next_landing));
  let request = LandingRequest {
    landing_id,
    // The grant binds the caller by its exact principal (its key), never by the session number several
    // principals can share, and the volume and snapshot this landing names (AUD-29-01).
    consumer: prepared.principal.key().into_boxed_slice(),
    volume: prepared.volume.bytes,
    snapshot: prepared.snapshot.value,
    source: prepared.source,
    grant: prepared.grant.map(GrantId),
    filter: prepared.filter.clone(),
    now_ns: state.clock.monotonic_ns(),
    media_durability: false,
    large_class_bytes: state.config.large_class_bytes,
    cores: 1,
    max_depth: 1,
    variance_permille: 0,
  };
  let slot = match state.volumes.get_mut(prepared.handle) {
    Ok(s) => s,
    Err(_) => return refused(Refusal::NotFound),
  };
  let was_overlay = slot.volume.is_overlay();
  // One host per volume: an overlay's base names its directories by the handles of the host its slot holds,
  // so the writer is built inside that host — taken now, as the landing runs (a granted landing's wait for
  // its lease left it serving), with the target walked again under the same containment — and given back
  // after. A writer over its own host would read the base through handles that name other directories there.
  let (mut os, land_target) = if was_overlay {
    let Some(host) = slot.host.take() else {
      return refused(Refusal::BaseUnavailable {
        path: prepared.target.clone(),
        errno: 0,
      });
    };
    match OsLand::open_target_in(host, std::path::Path::new(&prepared.target)) {
      Ok(pair) => pair,
      Err((host, refusal)) => {
        slot.host = Some(host);
        return refused(target_refusal(&refusal));
      }
    }
  } else {
    (prepared.os, prepared.land_target)
  };
  // Observe each entry to emit a `land.entry` span (§4.14), into a bounded buffer so a large landing
  // does not grow it without bound (ban 8). It records only timings; the spans are built and drained
  // into the shard's telemetry sink after the landing, once the borrows above are released.
  let telemetry_capacity = usize::try_from(state.config.region.slots)
    .unwrap_or(1)
    .max(1);
  let mut spans = SpanObserver::with_capacity(telemetry_capacity);
  let outcome = land(
    &mut os,
    &land_target,
    &mut slot.volume,
    &mut state.store,
    &mut state.landing.grants,
    lease,
    &mut state.landing.audit,
    &request,
    &mut spans,
  );
  drain_land_spans(state, spans);
  // The volume keeps the writer's host when it has a base: an overlay's own, given back; or — a scratch volume
  // the landing made an overlay over its target (§4.15 step 9) — the host whose handle now names its base
  // root. An overlay's walked target is closed first (its base root is its own handle), so a landing adds no
  // handle to the host it returns to. Until 2026-09-30 a landed scratch volume was served hostless: a landed
  // file, now a base entry, could not be read (found when the bridge began refusing hostless verbs on an
  // overlay, AUD-29-16).
  let mut rebased = false;
  if let Ok(slot) = state.volumes.get_mut(prepared.handle)
    && slot.volume.is_overlay()
  {
    rebased = !was_overlay;
    let mut host = os.into_host();
    if was_overlay {
      host.close_dir(land_target.dir);
    }
    slot.host = Some(host);
  }
  let ids = LandingIds {
    landing_id,
    fresh: presented.is_none(),
    volume: prepared.volume,
    snapshot: prepared.snapshot,
    target: &prepared.target,
    rebased,
  };
  let presenter = Presenter {
    client: prepared.client,
    principal: &prepared.principal,
  };
  match outcome {
    Ok(report) => finish(state, &prepared.principal, &ids, &report, prepared.grant),
    Err(refusal) => not_landed(state, presenter, &ids, refusal),
  }
}

/// The reply to a landing the engine did not land: its presentation when it needs a grant, else the typed
/// refusal.
#[cfg(unix)]
fn not_landed(
  state: &mut ShardState,
  presenter: Presenter<'_>,
  ids: &LandingIds<'_>,
  refusal: LandingRefusal,
) -> ReplyBody {
  match refusal {
    LandingRefusal::GrantRequired(plan) => {
      present(state, presenter, ids, &plan.manifest, &plan.binding)
    }
    // The engine hands back every entry's report; the refusal names the conflicting ones alone (§4.15: a conflict
    // refuses the whole landing, and the person resolving it needs only the entries in conflict). Before 2026-10-06
    // every entry was named, so an entry that would only have been created read as a conflict.
    LandingRefusal::Conflict(entries) => refused(Refusal::LandingConflict {
      entries: entries
        .iter()
        .filter(|e| matches!(e.verdict, Some(slates_land::verdict::Verdict::Conflict(_))))
        .map(|e| e.path.to_string())
        .collect(),
    }),
    LandingRefusal::LeaseHeld(held) => refused(Refusal::LandingLeaseHeld {
      holder: held.holder,
    }),
    // The lease ended while the landing waited to run: nothing was written.
    LandingRefusal::LeaseRequired => refused(Refusal::LandingLeaseLost),
    LandingRefusal::Grant(GrantRefusal::GrantMismatch { .. }) => refused(Refusal::GrantMismatch),
    // A grant approved for another consumer, volume, snapshot or target: the landing is not the one the
    // human approved, refused before any write and counted by the field that differed.
    LandingRefusal::Grant(GrantRefusal::Unbound { field }) => {
      state.count(unbound_refusal_name(field), 1);
      refused(Refusal::GrantMismatch)
    }
    LandingRefusal::Grant(_) => refused(Refusal::GrantInvalid),
    LandingRefusal::Target(e) => refused(Refusal::TargetUnavailable {
      reason: format!("{e:?}"),
    }),
    LandingRefusal::Volume(e) => {
      state.count(LANDING_VOLUME_REFUSED, 1);
      refused(refusal_of_vfs(&e))
    }
    // A run stepped after it ended: the server steps each run to its end once, so this names a defect of
    // the server's own driving, refused typed rather than answered as a landing.
    LandingRefusal::Ended => refused(Refusal::Unsupported {
      feature: "stepping a landing that had ended".to_owned(),
    }),
    // The volume's base host was not in its slot when the landing's slice came (a concurrent verb held it):
    // the landing ends as a crash leaves it, its entries in the overlay for a resume.
    LandingRefusal::HostAway => refused(Refusal::BaseUnavailable {
      path: ids.target.to_owned(),
      errno: 0,
    }),
  }
}

/// Starts a granted landing as an owned task on this shard and defers the verb's reply to it (§4.15
/// "Ownership facts": "runs as one of its tasks"; AUD-29-03). The task takes the target's lease from the
/// control shard under the target's canonical identity, runs the engine under it with the landing's records
/// and the request's completion committed as one atom, releases the lease, and then delivers the reply.
/// Before 2026-09-29 the landing ran inside the verb and took a lease from its own shard's table, keyed by
/// the target's path.
#[cfg(unix)]
fn defer_granted_landing(state: &mut ShardState, mut prepared: Prepared) -> ReplyBody {
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a granted landing outside a recorded request".to_owned(),
    });
  };
  let key = match prepared.os.fingerprint_dir(prepared.land_target.dir) {
    Ok(identity) => slates_land::grant::lease_key(&TargetIdentity {
      key: prepared.land_target.key.clone(),
      device: identity.dev,
      inode: identity.ino,
    }),
    Err(e) => {
      return refused(Refusal::TargetUnavailable {
        reason: format!("{e:?}"),
      });
    }
  };
  // The attempt is numbered in the landing-id shape — its partition in the high bits — so attempts of two
  // shards never share a holder.
  let holder = crate::verbs::landing_id(state.partition, state.landing.next_holder);
  state.landing.next_holder = state.landing.next_holder.saturating_add(1);
  let shard = state.shard;
  let control = state.shards.first().copied().unwrap_or(shard);
  // The term is the operator's failover bound, as the per-shard lease's was; the measured landing duration
  // p99 x k of §4.15's table, with its keepalive, comes with the sliced engine (AUD-29-25).
  let term = state.config.failover_slo_ns;
  let deadline = state.clock.monotonic_ns().saturating_add(term);
  let (origin, id) = request;
  let flight = (origin, id.client, id.sequence);
  // The verb's span is the cause of the landing's `land.entry` spans, though they end after it (§4.14).
  let cause = state.current_span;
  let route = state.reply_route.take();
  // One granted landing of a volume at a time: the landing runs in slices with the shard serving between
  // them, and two would advance one overlay (AUD-29-25).
  let volume = prepared.volume;
  if let Some(running) = state.landing.running.get(&volume) {
    return refused(Refusal::LandingLeaseHeld { holder: *running });
  }
  let task = async move {
    let taken = acquire_target_lease(
      LeaseAsk {
        shard,
        control,
        holder,
        term,
        deadline,
      },
      key.clone(),
    )
    .await;
    let held = taken.is_ok();
    let renew = LeaseAsk {
      shard,
      control,
      holder,
      term,
      deadline,
    };
    let finished =
      drive_granted_landing(prepared, taken, request, cause, (renew, key.clone())).await;
    // The lease goes before the reply, so the caller's next landing into the target finds it free.
    let released = !held
      || crate::xshard::call_within(
        shard,
        control,
        move |s| release_target_lease(s, &key, holder),
        term,
      )
      .await
      .is_some_and(|released| released.is_ok());
    crate::state::with_state(move |s| {
      s.landing.running.remove(&volume);
      if !released {
        // Unreleased, the lease ends by its term; counted, never silent.
        s.count(LEASE_UNRELEASED, 1);
      }
      if let Some((reply, route)) = finished {
        deliver_landing(s, reply, route);
      }
    });
  };
  match slates_rt::futures::spawn(task).and_then(slates_rt::futures::detach) {
    Ok(()) => {
      state.landing.in_flight.insert(flight, route);
      state.landing.running.insert(volume, holder);
      state.acceptance_deferred = true;
      deferred_reply()
    }
    Err(_) => refused(Refusal::Overloaded { shard }),
  }
}

/// A granted landing between its slices on its owner shard (AUD-29-25; §4.15, §4.3): the engine's run, and
/// what its reply and records need. The run owns its writer; an overlay volume's base host goes back to the
/// volume's slot between slices (with the writer's own state kept `away`), so the volume serves its base
/// while the landing waits.
#[cfg(unix)]
struct GrantedRun {
  run: LandingRun<'static, OsLand>,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  landing_id: u64,
  fresh: bool,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: String,
  principal: Principal,
  client: u32,
  grant: Option<u64>,
  was_overlay: bool,
  target_dir: slates_vfs::host::HostDir,
  spans: SpanObserver,
  away: Option<Away>,
  /// An unnamed landing's snapshot of the head, taken when it began and destroyed when it ends: what lands is
  /// the head as it was then, while writers go on between slices (§4.15; A-48/49 land a snapshot exactly).
  implicit: Option<slates_vfs::ids::SnapshotId>,
}

/// A granted landing that ended before running: what its reply needs.
#[cfg(unix)]
struct NotRun {
  refusal: LandingRefusal,
  landing_id: u64,
  fresh: bool,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: String,
  principal: Principal,
  client: u32,
}

/// Where beginning a granted landing left it.
#[cfg(unix)]
enum Begun {
  /// Running: stepped in slices.
  Running(Box<GrantedRun>),
  /// Refused before running (its lease, the volume, the target, the grant): the reply is built where its
  /// completion is recorded.
  NotRun(Box<NotRun>),
  /// Refused before anything about the landing was known (no lease, no volume): the reply itself.
  Refused(ReplyBody),
}

/// What one slice left the run at.
#[cfg(unix)]
enum Stepped {
  /// More host work remains.
  More,
  /// Only the finish remains.
  Ready,
  /// The run ended early with this outcome (a refusal).
  Ended(Box<Result<LandingReport, LandingRefusal>>),
}

/// Drives a granted landing under its lease: begins it, steps it one slice per turn of the shard (yielding
/// between, so the shard serves its other clients, lease checks and shutdown while a large landing runs),
/// and finishes it where its records and the request's completion commit as one atom.
#[cfg(unix)]
async fn drive_granted_landing(
  prepared: Prepared,
  lease: Result<LandingLease, Refusal>,
  request: (u64, slates_wire::request::RequestId),
  cause: Option<slates_wire::observe::SpanContext>,
  (renew, key): (LeaseAsk, String),
) -> Option<(ReplyBody, Option<crate::merge_service::ReplyRoute>)> {
  let begun = crate::state::with_state(move |s| begin_granted(s, prepared, lease))?;
  let mut granted = match begun {
    Begun::Running(granted) => granted,
    Begun::NotRun(not_run) => {
      return crate::state::with_state(move |s| {
        complete_landing_with(s, request, cause, |s| reply_not_run(s, *not_run))
      });
    }
    Begun::Refused(reply) => {
      return crate::state::with_state(move |s| {
        complete_landing_with(s, request, cause, |_| reply)
      });
    }
  };
  loop {
    let stepped = crate::state::with_state(|s| {
      let stepped = step_granted(s, &mut granted);
      // A recall a slice asked for is sent now, as a verb's is after its transaction (A-79): the landing runs as a
      // task, after its verb returned.
      crate::delegation::drain(s);
      stepped
    })?;
    match stepped {
      Stepped::More => {
        keep_lease_alive(&mut granted, renew, &key).await;
        slates_rt::futures::yield_now().await;
      }
      Stepped::Ready => {
        recall_before_finish(&mut granted, renew, &key).await;
        return crate::state::with_state(move |s| {
          complete_landing_with(s, request, cause, |s| finish_granted(s, *granted, None))
        });
      }
      Stepped::Ended(ended) => {
        return crate::state::with_state(move |s| {
          complete_landing_with(s, request, cause, |s| {
            finish_granted(s, *granted, Some(*ended))
          })
        });
      }
    }
  }
}

/// The finish advances the volume's overlay past what reached the disk, which changes the landed files, so every
/// delegation held on a file of the volume is recalled first (RFC 8881 §10.2; A-79) and the landing waits for their
/// return, parked on the recall gate and keeping its target lease alive. A delegation not returned within a lease of
/// the recall is revoked (§10.4.5), by the drain at the deadline, and the finish runs. Before 2026-10-05 the finish
/// met the gate and the granted landing answered `Unpublished`, which `slates land` does not retry, and the recall it
/// asked for was never sent, since nothing drained after a landing task's slices.
#[cfg(unix)]
async fn recall_before_finish(granted: &mut GrantedRun, renew: LeaseAsk, key: &str) {
  let handle = granted.handle;
  let mut deadline: Option<u64> = None;
  loop {
    let asked = crate::state::with_state(|s| {
      let prefix = s.volumes.get(handle).ok()?.volume.prefix();
      if !s.store.recall_gate.recall_volume(prefix) {
        return None;
      }
      crate::delegation::drain(s);
      s.count(LANDING_HELD_FOR_RECALL, 1);
      Some((s.store.recall_gate.generation(), s.config.failover_slo_ns))
    })
    .flatten();
    let Some((generation, lease)) = asked else {
      return;
    };
    let now = slates_rt::futures::now_ns();
    // §10.4.5's "after a lease" is strict, and the recall was sent after this first ask began.
    let until = *deadline.get_or_insert(now.saturating_add(lease).saturating_add(1));
    if now >= until {
      // The lease passed: the drain revokes what lapsed, and the finish runs over what is left.
      let _ = crate::state::with_state(crate::delegation::drain);
      return;
    }
    let parked = slates_rt::futures::within(
      until.saturating_sub(now),
      std::future::poll_fn(|cx| {
        let released = crate::state::with_state(|s| {
          s.store.recall_gate.wait_for_release(generation, cx.waker())
        });
        match released {
          Some(false) => std::task::Poll::Pending,
          _ => std::task::Poll::Ready(()),
        }
      }),
    )
    .await;
    keep_lease_alive(granted, renew, key).await;
    if parked.is_err() {
      return;
    }
  }
}

/// Counter: a granted landing that waited before its finish for delegations held on its volume's files (A-79).
/// Format: a counter name in the daemon's status report.
#[cfg(unix)]
const LANDING_HELD_FOR_RECALL: &str = "landing.held_for_recall";

/// Format: how much of its term a landing's lease has left when the landing renews it, in permille — half:
/// the margin then covers a slice and the renewal's round trip to the control shard, each far shorter than
/// the term, while a landing renews at most twice a term.
#[cfg(unix)]
const RENEW_AT_REMAINING_PERMILLE: u64 = 500;

/// The keepalive between a landing's slices (AUD-29-25; the lease records waited for it): once half its
/// lease's term has passed, the landing re-takes the lease on the control shard as its own holder and runs
/// under the renewed term. A lease another attempt took meanwhile (this one's ended) is not renewed; the run's
/// own per-entry fence then stops its writes, as before. Counted either way.
#[cfg(unix)]
async fn keep_lease_alive(granted: &mut GrantedRun, ask: LeaseAsk, key: &str) {
  let expires_ns = granted.run.lease().expires_ns;
  let margin = ask.term.saturating_mul(RENEW_AT_REMAINING_PERMILLE) / PERMILLE;
  let Some(now) = crate::state::with_state(|s| s.clock.monotonic_ns()) else {
    return;
  };
  if now.saturating_add(margin) < expires_ns {
    return;
  }
  let target = key.to_owned();
  let deadline = now.saturating_add(ask.term);
  let renewed = crate::xshard::call_within(
    ask.shard,
    ask.control,
    move |s| take_target_lease(s, target, ask.holder, ask.term, deadline),
    ask.term,
  )
  .await;
  let kept = match renewed {
    Some(Ok(lease)) => granted.run.renew_lease(lease),
    _ => false,
  };
  crate::state::with_state(|s| {
    let counter = if kept {
      LEASE_RENEWED
    } else {
      LEASE_NOT_RENEWED
    };
    s.count(counter, 1);
  });
}

/// Format: the landing-plane counter of a lease a running landing renewed between its slices.
#[cfg(unix)]
const LEASE_RENEWED: &str = "landing.lease_renewed";

/// Format: the landing-plane counter of a lease a running landing could not renew (another attempt holds the
/// target, or the control shard did not answer): its term then ends the landing's writes.
#[cfg(unix)]
const LEASE_NOT_RENEWED: &str = "landing.lease_not_renewed";

/// Begins a granted landing on its owner shard: its id and request, the head's implicit snapshot for an
/// unnamed landing, the writer (inside an overlay's own base host), and the engine's plan, grant and lease
/// checks.
#[cfg(unix)]
fn begin_granted(
  state: &mut ShardState,
  prepared: Prepared,
  lease: Result<LandingLease, Refusal>,
) -> Begun {
  let lease = match lease {
    Ok(lease) => lease,
    Err(refusal) => return Begun::Refused(refused(refusal)),
  };
  let presented = prepared.grant.and_then(|g| presentation_for(state, g));
  let landing_id = presented
    .unwrap_or_else(|| crate::verbs::landing_id(state.partition, state.landing.next_landing));
  let Ok(slot) = state.volumes.get_mut(prepared.handle) else {
    return Begun::Refused(refused(Refusal::NotFound));
  };
  // An unnamed landing lands the head as it is now: a snapshot of it, which the writers that go on
  // between slices do not move.
  let (source, implicit) = match prepared.source {
    Source::Head => match slot.volume.snapshot(&mut state.store) {
      Ok(id) => (Source::Snapshot(id), Some(id)),
      Err(e) => {
        state.count(LANDING_SNAPSHOT_REFUSED, 1);
        return Begun::Refused(refused(refusal_of_vfs(&e)));
      }
    },
    named => (named, None),
  };
  let request = LandingRequest {
    landing_id,
    consumer: prepared.principal.key().into_boxed_slice(),
    volume: prepared.volume.bytes,
    snapshot: prepared.snapshot.value,
    source,
    grant: prepared.grant.map(GrantId),
    filter: prepared.filter.clone(),
    now_ns: state.clock.monotonic_ns(),
    media_durability: false,
    large_class_bytes: state.config.large_class_bytes,
    cores: 1,
    max_depth: 1,
    variance_permille: 0,
  };
  let was_overlay = slot.volume.is_overlay();
  let not_run = |refusal: LandingRefusal, principal: Principal| {
    Begun::NotRun(Box::new(NotRun {
      refusal,
      landing_id,
      fresh: presented.is_none(),
      volume: prepared.volume,
      snapshot: prepared.snapshot,
      target: prepared.target.clone(),
      principal,
      client: prepared.client,
    }))
  };
  let (os, land_target) = if was_overlay {
    let Some(host) = slot.host.take() else {
      drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
      return Begun::Refused(refused(Refusal::BaseUnavailable {
        path: prepared.target.clone(),
        errno: 0,
      }));
    };
    match OsLand::open_target_in(host, std::path::Path::new(&prepared.target)) {
      Ok(pair) => pair,
      Err((host, refusal)) => {
        slot.host = Some(host);
        drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
        return Begun::Refused(refused(target_refusal(&refusal)));
      }
    }
  } else {
    (prepared.os, prepared.land_target)
  };
  let begun = begin_landing(
    HostSlot::Owned(os),
    &land_target,
    &mut slot.volume,
    &mut state.store,
    &mut state.landing.grants,
    Some(&lease),
    &mut state.landing.audit,
    &request,
  );
  let telemetry_capacity = usize::try_from(state.config.region.slots)
    .unwrap_or(1)
    .max(1);
  match begun {
    Ok(run) => {
      let mut granted = GrantedRun {
        run,
        handle: prepared.handle,
        landing_id,
        fresh: presented.is_none(),
        volume: prepared.volume,
        snapshot: prepared.snapshot,
        target: prepared.target.clone(),
        principal: prepared.principal.clone(),
        client: prepared.client,
        grant: prepared.grant,
        was_overlay,
        target_dir: land_target.dir,
        spans: SpanObserver::with_capacity(telemetry_capacity),
        away: None,
        implicit,
      };
      if was_overlay {
        lend_back(slot, &mut granted);
      }
      Begun::Running(Box::new(granted))
    }
    Err((host, refusal)) => {
      give_host_back(slot, host, was_overlay, land_target.dir);
      drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
      not_run(refusal, prepared.principal.clone())
    }
  }
}

/// Gives an overlay's base host back to its volume's slot between slices, keeping the writer's own state.
#[cfg(unix)]
fn lend_back(slot: &mut crate::state::VolumeSlot, granted: &mut GrantedRun) {
  if let Some(os) = granted.run.take_host() {
    let (host, away) = os.lend();
    slot.host = Some(host);
    granted.away = Some(away);
  }
}

/// Takes an overlay's base host from its volume's slot back into the run for a slice; whether it could.
#[cfg(unix)]
fn take_for_slice(slot: &mut crate::state::VolumeSlot, granted: &mut GrantedRun) -> bool {
  match (slot.host.take(), granted.away.take()) {
    (Some(host), Some(away)) => {
      granted.run.put_host(OsLand::resume(host, away));
      true
    }
    (host, away) => {
      slot.host = host;
      granted.away = away;
      false
    }
  }
}

/// The writer's host after a landing that did not run: an overlay's own goes back to its slot, its walked
/// target closed; a scratch volume's writer closes with it.
#[cfg(unix)]
fn give_host_back(
  slot: &mut crate::state::VolumeSlot,
  host: HostSlot<'static, OsLand>,
  was_overlay: bool,
  target_dir: slates_vfs::host::HostDir,
) {
  if let (true, HostSlot::Owned(os)) = (was_overlay, host) {
    let mut host = os.into_host();
    host.close_dir(target_dir);
    slot.host = Some(host);
  }
}

/// Destroys an unnamed landing's implicit snapshot of the head; a refusal is counted, never silent (the
/// snapshot then stays as any snapshot does, until destroyed).
#[cfg(unix)]
fn drop_implicit(
  slot: &mut crate::state::VolumeSlot,
  store: &mut slates_vfs::volume::Store,
  implicit: Option<slates_vfs::ids::SnapshotId>,
  counters: &mut std::collections::BTreeMap<&'static str, u64>,
) {
  if let Some(id) = implicit
    && slot.volume.destroy_snapshot(store, id).is_err()
  {
    let kept = counters.entry(IMPLICIT_SNAPSHOT_KEPT).or_insert(0);
    *kept = kept.saturating_add(1);
  }
}

/// Format: the landing-plane counter of an unnamed landing's implicit snapshot whose destroy was refused.
#[cfg(unix)]
const IMPLICIT_SNAPSHOT_KEPT: &str = "landing.implicit_snapshot_kept";
/// Format: the refusal-ledger names of a landing refused at each of its volume stages — the implicit snapshot
/// of the head, the engine's read of the volume, the publication after it — so a refusal names its stage
/// (a `NoSpace` seen only on the macOS runner, 2026-10-01, carried no stage).
#[cfg(unix)]
const LANDING_SNAPSHOT_REFUSED: &str = "landing.snapshot_refused";
/// Format: see [`LANDING_SNAPSHOT_REFUSED`].
#[cfg(unix)]
const LANDING_VOLUME_REFUSED: &str = "landing.volume_refused";
/// Format: see [`LANDING_SNAPSHOT_REFUSED`].
#[cfg(unix)]
const LANDING_PUBLISH_REFUSED: &str = "landing.publish_refused";

/// Format: the share of the shard's step quantum one landing slice takes, in permille — half, as the
/// archive walk's slice takes (`config::archive_slice_bytes`), so a slice leaves the rest of the shard's
/// step to its other work.
#[cfg(unix)]
const LANDING_SLICE_PERMILLE: u64 = 500;
/// Format: the permille scale.
#[cfg(unix)]
const PERMILLE: u64 = 1_000;

/// One slice of a granted landing: the run stepped for its share of the shard's quantum, the base host lent
/// back after. A volume gone meanwhile (destroyed) or whose base host is away ends the run, its entries left in
/// the overlay as a crash leaves them.
#[cfg(unix)]
fn step_granted(state: &mut ShardState, granted: &mut GrantedRun) -> Stepped {
  let budget_ns = state
    .config
    .step_quantum_ns()
    .saturating_mul(LANDING_SLICE_PERMILLE)
    / PERMILLE;
  let Ok(slot) = state.volumes.get_mut(granted.handle) else {
    granted.run.abandon();
    return Stepped::Ended(Box::new(Err(LandingRefusal::Volume(
      slates_vfs::error::VfsError::StaleHandle,
    ))));
  };
  if granted.was_overlay && !take_for_slice(slot, granted) {
    granted.run.abandon();
    return Stepped::Ended(Box::new(Err(LandingRefusal::HostAway)));
  }
  let began = state.clock.monotonic_ns();
  let stepped = granted.run.step(
    &mut slot.volume,
    &mut state.store,
    &mut state.landing.audit,
    &mut granted.spans,
    budget_ns,
  );
  if granted.was_overlay {
    lend_back(slot, granted);
  }
  let took = state.clock.monotonic_ns().saturating_sub(began);
  state.landing.slice_times.record(took);
  state.landing.slice_budget_ns = budget_ns;
  // A step returns once its budget has passed, so it may overrun by its last unit and no more.
  if granted.run.last_step_ns() > budget_ns.saturating_add(granted.run.last_unit_ns()) {
    state.landing.slices_past_budget = state.landing.slices_past_budget.saturating_add(1);
  }
  match stepped {
    Some(ended) => Stepped::Ended(Box::new(ended)),
    None if granted.run.ready_to_finish() => Stepped::Ready,
    None => Stepped::More,
  }
}

/// Finishes a granted landing inside its completion's atom: the run's finish (unless it `ended` early), its
/// once-grant consumed, its spans drained, the writer's host settled, the implicit snapshot destroyed, and
/// the reply with the landing's records.
#[cfg(unix)]
fn finish_granted(
  state: &mut ShardState,
  mut granted: GrantedRun,
  ended: Option<Result<LandingReport, LandingRefusal>>,
) -> ReplyBody {
  let outcome = match ended {
    Some(ended) => ended,
    None => match state.volumes.get_mut(granted.handle) {
      Ok(slot) => {
        if !granted.was_overlay || take_for_slice(slot, &mut granted) {
          granted.run.finish(
            &mut slot.volume,
            &mut state.store,
            &mut state.landing.audit,
            &mut granted.spans,
          )
        } else {
          granted.run.abandon();
          Err(LandingRefusal::HostAway)
        }
      }
      Err(_) => {
        granted.run.abandon();
        Err(LandingRefusal::Volume(
          slates_vfs::error::VfsError::StaleHandle,
        ))
      }
    },
  };
  settle_grant(&mut state.landing.grants, granted.run.grant(), &outcome);
  let spans = std::mem::replace(&mut granted.spans, SpanObserver::with_capacity(1));
  drain_land_spans(state, spans);
  let mut rebased = false;
  if let Ok(slot) = state.volumes.get_mut(granted.handle) {
    if slot.volume.is_overlay() {
      rebased = !granted.was_overlay;
      if let Some(os) = granted.run.take_host() {
        let mut host = os.into_host();
        if granted.was_overlay {
          host.close_dir(granted.target_dir);
        }
        slot.host = Some(host);
      }
    }
    drop_implicit(
      slot,
      &mut state.store,
      granted.implicit,
      &mut state.refusals,
    );
  }
  let ids = LandingIds {
    landing_id: granted.landing_id,
    fresh: granted.fresh,
    volume: granted.volume,
    snapshot: granted.snapshot,
    target: &granted.target,
    rebased,
  };
  let presenter = Presenter {
    client: granted.client,
    principal: &granted.principal,
  };
  match outcome {
    Ok(report) => finish(state, &granted.principal, &ids, &report, granted.grant),
    Err(refusal) => not_landed(state, presenter, &ids, refusal),
  }
}

/// The reply to a granted landing refused before it ran.
#[cfg(unix)]
fn reply_not_run(state: &mut ShardState, not_run: NotRun) -> ReplyBody {
  let ids = LandingIds {
    landing_id: not_run.landing_id,
    fresh: not_run.fresh,
    volume: not_run.volume,
    snapshot: not_run.snapshot,
    target: &not_run.target,
    rebased: false,
  };
  let presenter = Presenter {
    client: not_run.client,
    principal: &not_run.principal,
  };
  not_landed(state, presenter, &ids, not_run.refusal)
}

/// Where a landing attempt asks for its target's lease: from this shard to the control shard, for the
/// attempt `holder`, for `term` nanoseconds, answered by the monotonic `deadline`.
#[cfg(unix)]
#[derive(Clone, Copy)]
struct LeaseAsk {
  shard: u16,
  control: u16,
  holder: u64,
  term: u64,
  deadline: u64,
}

/// Takes the target lease `key` from the control shard for a landing attempt, waiting at most one term. A
/// take whose answer does not come back in that time is compensated — the attempt's own lease released,
/// should the take have landed; a take still queued refuses itself past the deadline — so a lease outlives
/// an attempt that never learned of it only when the compensation fails as well, and then by its term.
#[cfg(unix)]
async fn acquire_target_lease(ask: LeaseAsk, key: String) -> Result<LandingLease, Refusal> {
  let LeaseAsk {
    shard,
    control,
    holder,
    term,
    deadline,
  } = ask;
  let asked = key.clone();
  if let Some(taken) = crate::xshard::call_within(
    shard,
    control,
    move |s| take_target_lease(s, asked, holder, term, deadline),
    term,
  )
  .await
  {
    return taken;
  }
  let compensated = crate::xshard::call_within(
    shard,
    control,
    move |s| release_target_lease(s, &key, holder),
    term,
  )
  .await
  .is_some_and(|released| released.is_ok());
  crate::state::with_state(|s| {
    s.count(LEASE_TAKE_UNANSWERED, 1);
    if !compensated {
      s.count(LEASE_UNRELEASED, 1);
    }
  });
  Err(Refusal::Overloaded { shard: control })
}

/// Format: the landing-plane counter of a target-lease take whose answer did not come back within its term.
#[cfg(unix)]
const LEASE_TAKE_UNANSWERED: &str = "landing.lease_take_unanswered";

/// Format: the landing-plane counter of a target lease whose release did not reach the control shard (it
/// ends by its term).
#[cfg(unix)]
const LEASE_UNRELEASED: &str = "landing.lease_unreleased";

/// The placeholder a deferred landing's verb returns. It is never delivered: the verb deferred its reply, and
/// `run_recorded` answers nothing for it.
#[cfg(unix)]
pub(crate) fn deferred_reply() -> ReplyBody {
  ReplyBody::Landed {
    outcome: LandingOutcome {
      landing: 0,
      state: String::new(),
      written: 0,
      skipped: 0,
      conflicts: 0,
      failed: 0,
      bytes_written: 0,
      held: 0,
      durability: LandingDurability {
        data_synced: false,
        dirs_synced: false,
        media: false,
        media_requested: false,
        dirs: 0,
      },
      degraded: Vec::new(),
      ramp_depth: 0,
    },
  }
}

/// A granted landing's completion on its owner shard: the reply `produce` builds — the finished landing's
/// records, or a refusal — and the request's completion committed as one atom. Returns the recorded reply
/// and where it goes.
#[cfg(unix)]
fn complete_landing_with(
  state: &mut ShardState,
  (origin, id): (u64, slates_wire::request::RequestId),
  cause: Option<slates_wire::observe::SpanContext>,
  produce: impl FnOnce(&mut ShardState) -> ReplyBody,
) -> (ReplyBody, Option<crate::merge_service::ReplyRoute>) {
  let route = state
    .landing
    .in_flight
    .remove(&(origin, id.client, id.sequence))
    .flatten();
  let outer = std::mem::replace(&mut state.current_span, cause);
  state.db.begin();
  let reply = produce(state);
  let recorded = crate::verbs::record_completion(state, origin, id, reply);
  let reply = match state.db.commit(&mut state.segment) {
    Ok(_) => recorded,
    Err(e) => {
      // Nothing of the landing's records is durable, as in `run_recorded`: counted, never recorded as the
      // completion, so a retry runs again.
      let refusal = crate::error::refusal_of_db(&e);
      state.count(crate::verbs::refusal_name(&refusal), 1);
      crate::verbs::reconcile_unpublished_effects(state);
      refused(refusal)
    }
  };
  state.current_span = outer;
  (reply, route)
}

/// Delivers a granted landing's reply to where its request waits, on its client's shard. A request with no
/// route — a forward from another node — reads its completion record instead.
#[cfg(unix)]
fn deliver_landing(
  state: &mut ShardState,
  reply: ReplyBody,
  route: Option<crate::merge_service::ReplyRoute>,
) {
  let Some(route) = route else {
    return;
  };
  let delivery = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      crate::state::deliver(route.client_index, route.request, reply, true);
    }),
    None,
  );
  if slates_rt::registry::send_control(
    route.shard,
    slates_rt::control::Control::Spawn(Box::new(delivery)),
  )
  .is_err()
  {
    // The client's retry answers from the completion record.
    state.count(LANDING_UNDELIVERED, 1);
  }
}

/// Format: the landing-plane counter of a granted landing's reply that could not be handed to its client's
/// shard.
#[cfg(unix)]
const LANDING_UNDELIVERED: &str = "landing.reply_undelivered";

/// The refusal-ledger name of a landing refused for a grant bound elsewhere, by the field that differed.
#[cfg(unix)]
fn unbound_refusal_name(field: slates_land::grant::BindingField) -> &'static str {
  use slates_land::grant::BindingField;
  match field {
    BindingField::Consumer => "grant_unbound.consumer",
    BindingField::Volume => "grant_unbound.volume",
    BindingField::Snapshot => "grant_unbound.snapshot",
    BindingField::Target => "grant_unbound.target",
  }
}

/// The state a landing of a named snapshot lands (§4.15 step 1; AUD-29-02, A-49): that snapshot, exactly
/// as it froze. A snapshot the catalog does not hold for this volume — never taken, destroyed, or another
/// volume's — is `NotFound`, before any host access. Before 2026-09-30 the engine planned and wrote the head
/// whatever snapshot was named, and a head changed since the snapshot was refused instead.
#[cfg(unix)]
fn named_snapshot_source(
  state: &ShardState,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  record: &slates_db::catalog::VolumeRecord,
  snapshot: SnapshotId,
) -> Result<Source, Refusal> {
  let db_snapshot = to_db_snapshot(snapshot);
  if state
    .db
    .partition()
    .snapshot(record.id, db_snapshot)
    .is_none()
  {
    return Err(Refusal::NotFound);
  }
  let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
  let vfs_snapshot = slates_vfs::ids::SnapshotId {
    index: u32::try_from(db_snapshot.value >> u32::BITS).map_err(|_| Refusal::NotFound)?,
    generation: u32::try_from(db_snapshot.value & u64::from(u32::MAX))
      .map_err(|_| Refusal::NotFound)?,
  };
  slot
    .volume
    .snapshot_info(vfs_snapshot)
    .map_err(|_| Refusal::NotFound)?;
  Ok(Source::Snapshot(vfs_snapshot))
}

/// Records the planned landing (AwaitingGrant) and its audit, and replies `GrantRequired`.
/// Who presents a landing: the calling client (whose retirement abandons the presentation) and its
/// principal.
#[cfg(unix)]
struct Presenter<'a> {
  client: u32,
  principal: &'a Principal,
}

/// Whether a presentation by `client` of `volume` into `target` would pass the shard's bound: a client's
/// re-presentation of the same volume and target replaces its own and never does (AUD-29-07).
#[cfg(unix)]
fn presentations_full(state: &ShardState, client: u32, volume: DbVolumeId, target: &str) -> bool {
  let replaces = state.landing.awaiting.values().any(|awaiting| {
    awaiting.client == client && awaiting.volume == volume && awaiting.target == target
  });
  !replaces && state.landing.awaiting.len() >= state.config.landings_awaiting_per_shard
}

/// The presentation a granted landing lands (AUD-29-07): the awaiting landing whose manifest and binding the
/// grant was issued from. `None` for a grant covering a plan no presentation made (a session grant's later
/// plans), which lands under a fresh id.
#[cfg(unix)]
fn presentation_for(state: &ShardState, grant: u64) -> Option<u64> {
  let granted = state.landing.grants.get(GrantId(grant))?;
  state
    .landing
    .awaiting
    .iter()
    .find(|(_, awaiting)| {
      awaiting.manifest == granted.manifest && awaiting.binding == granted.binding
    })
    .map(|(id, _)| *id)
}

#[cfg(unix)]
fn present(
  state: &mut ShardState,
  presenter: Presenter<'_>,
  ids: &LandingIds<'_>,
  manifest: &Manifest,
  binding: &slates_land::grant::GrantBinding,
) -> ReplyBody {
  let Presenter { client, principal } = presenter;
  state.landing.next_landing = state.landing.next_landing.saturating_add(1);
  let hash = manifest.hash;
  let landing_id = ids.landing_id;
  // A client presenting the same volume and target again replaces its earlier presentation, whose id is no
  // longer grantable (AUD-29-07): re-planning costs no room.
  state.landing.awaiting.retain(|_, awaiting| {
    !(awaiting.client == client && awaiting.volume == ids.volume && awaiting.target == ids.target)
  });
  state.landing.awaiting.insert(
    landing_id,
    Awaiting {
      manifest: hash,
      binding: binding.clone(),
      volume: ids.volume,
      snapshot: ids.snapshot,
      target: ids.target.to_owned(),
      principal: principal.clone(),
      client,
    },
  );
  let now = state.clock.monotonic_ns();
  let record = LandingRecord {
    id: landing_id,
    volume: ids.volume,
    snapshot: ids.snapshot,
    target: ids.target.to_owned(),
    manifest: hash,
    grant: None,
    state: DbLandingState::AwaitingGrant,
    written: 0,
    conflicts: 0,
  };
  let ops = [
    Op::LandingRecorded { record },
    Op::AuditAppended {
      record: DbAuditRecord {
        seq: 0,
        at_ns: now,
        kind: DbAuditKind::LandingPlanned,
        principal: principal.clone(),
        grant: None,
        landing: Some(landing_id),
        manifest: Some(hash),
        outcome: None,
      },
    },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return refused(crate::error::refusal_of_db(&e));
    }
  }
  ReplyBody::GrantRequired {
    landing: landing_id,
    manifest: hash,
    summary: summary_of(manifest),
    conflicts: Vec::new(),
  }
}

/// Persists a finished landing: the record, its terminal audit, and the consumed grant.
#[cfg(unix)]
fn finish(
  state: &mut ShardState,
  principal: &Principal,
  ids: &LandingIds<'_>,
  report: &LandingReport,
  grant: Option<u64>,
) -> ReplyBody {
  let landing_id = ids.landing_id;
  if ids.fresh {
    state.landing.next_landing = state.landing.next_landing.saturating_add(1);
  }
  // A finished landing consumes the presentation it landed; an aborted one keeps it, so the resume lands
  // under the same id and sweeps what the crashed attempt left (AUD-29-07).
  if matches!(
    report.state,
    slates_land::engine::LandingState::Done | slates_land::engine::LandingState::Partial
  ) {
    state.landing.awaiting.remove(&landing_id);
  }
  let now = state.clock.monotonic_ns();
  let db_state = db_landing_state(&report.state);
  let record = LandingRecord {
    id: landing_id,
    volume: ids.volume,
    snapshot: ids.snapshot,
    target: ids.target.to_owned(),
    manifest: report.manifest_hash,
    grant,
    state: db_state,
    written: u32::try_from(report.written).unwrap_or(u32::MAX),
    conflicts: u32::try_from(report.conflicts).unwrap_or(u32::MAX),
  };
  // A landing under a fresh id is recorded; one under its presentation's id moves that presentation's
  // record out of `AwaitingGrant` (AUD-29-07), its grant named by the audit records that follow.
  let mut ops = vec![if ids.fresh {
    Op::LandingRecorded { record }
  } else {
    Op::LandingStateChanged {
      id: landing_id,
      state: db_state,
      written: record.written,
      conflicts: record.conflicts,
    }
  }];
  // Mirror the engine's audit records for this landing into the durable log.
  for audit in state.landing.audit.records() {
    if audit.landing == landing_id {
      ops.push(Op::AuditAppended {
        record: db_audit(audit, principal),
      });
    }
  }
  ops.extend(closing_ops(state, ids, grant));
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return refused(crate::error::refusal_of_db(&e));
    }
  }
  if let Some(refusal) = publish_landed(state, ids, report) {
    return refusal;
  }
  ReplyBody::Landed {
    outcome: LandingOutcome {
      landing: landing_id,
      state: landing_state_name(db_state),
      written: report.written as u64,
      skipped: report.skipped as u64,
      conflicts: report.conflicts as u64,
      failed: report.failed as u64,
      bytes_written: report.bytes_written,
      held: u64::try_from(report.held).unwrap_or(u64::MAX),
      durability: durability_of(&report.durability),
      degraded: report.degraded.iter().map(degradation_of).collect(),
      ramp_depth: report.ramp_depth,
    },
  }
}

/// The ops that close a finished landing's records: its grant's transition, and the volume's new base when
/// the landing made a scratch volume an overlay.
#[cfg(unix)]
fn closing_ops(state: &ShardState, ids: &LandingIds<'_>, grant: Option<u64>) -> Vec<Op> {
  let mut ops = Vec::new();
  // The grant's transition as it happened (AUD-29-06): the engine consumes a single-use grant whose landing
  // finished and never a session grant, and a landing it aborted leaves either grant usable for the resume;
  // the durable state follows the runtime one. Until 2026-09-29 every grant presented was recorded consumed.
  if let Some(id) = grant
    && state
      .landing
      .grants
      .get(slates_land::grant::GrantId(id))
      .is_some_and(|g| g.state == LandGrantState::Consumed)
  {
    ops.push(Op::GrantStateChanged {
      id,
      state: DbGrantState::Consumed,
    });
  }
  // A landing that made a scratch volume an overlay records its new base (§4.15 step 9): until 2026-09-30 the
  // record kept `Scratch`, and a restart rebuilt the volume without the directory its landed files are in.
  if ids.rebased {
    ops.push(Op::VolumeRebased {
      id: ids.volume,
      base: slates_db::catalog::BaseRecord::Path {
        path: ids.target.to_owned(),
      },
    });
  }
  ops
}

/// Publishes the shard's content images after a landing that advanced the volume (see the body); the refusal
/// to reply with when the volume was not captured, else `None`.
#[cfg(unix)]
fn publish_landed(
  state: &mut ShardState,
  ids: &LandingIds<'_>,
  report: &LandingReport,
) -> Option<ReplyBody> {
  // A landing that advanced the volume (its landed entries left the overlay; a scratch volume took a base)
  // changed what a recovery rebuilds, so the content image is published before the reply, as every mutating
  // verb's is (§4.8, AUD-05): a volume the publish could not capture is refused, never acknowledged. Until
  // 2026-09-30 a landing published nothing, and a restart rebuilt the pre-landing image — refused outright
  // once the record named the new base (the image still said scratch).
  if matches!(
    report.state,
    slates_land::engine::LandingState::Done | slates_land::engine::LandingState::Partial
  ) {
    match crate::verbs::publish_shard(state) {
      Ok(published) if !published.captured(ids.volume) => {
        state.count(LANDING_PUBLISH_REFUSED, 1);
        return Some(refused(crate::error::refusal_of_vfs(
          &slates_vfs::VfsError::RecoveryIncomplete,
        )));
      }
      Ok(_) => {}
      Err(error) => {
        state.count(LANDING_PUBLISH_REFUSED, 1);
        return Some(refused(crate::error::refusal_of_vfs(&error)));
      }
    }
  }
  None
}

/// The engine's durability as the reply carries it (AUD-29-05: until 2026-09-29 the reply dropped it).
#[cfg(unix)]
fn durability_of(durability: &Durability) -> LandingDurability {
  LandingDurability {
    data_synced: durability.data_synced,
    dirs_synced: durability.dirs_synced,
    media: durability.media,
    media_requested: durability.media_requested,
    dirs: u64::try_from(durability.dirs).unwrap_or(u64::MAX),
  }
}

/// A Degraded cell the engine met, as the reply carries it (AUD-29-05: until 2026-09-29 the reply dropped
/// every one).
#[cfg(unix)]
fn degradation_of(degradation: &Degradation) -> LandingDegradation {
  match degradation {
    Degradation::NoExchange { widest_window_ns } => LandingDegradation::NoExchange {
      widest_window_ns: *widest_window_ns,
    },
    Degradation::BarriersOnly => LandingDegradation::BarriersOnly,
    Degradation::Crashed { errno } => LandingDegradation::Crashed { errno: *errno },
    Degradation::Unsynced { dir, error } => LandingDegradation::Unsynced {
      dir: dir.to_string(),
      answer: answer_of(*error),
    },
    Degradation::MediaUnsynced { error } => LandingDegradation::MediaUnsynced {
      answer: answer_of(*error),
    },
    Degradation::Leftover { path, error } => LandingDegradation::Leftover {
      path: path.to_string(),
      answer: answer_of(*error),
    },
    Degradation::Unswept { dir, error } => LandingDegradation::Unswept {
      dir: dir.to_string(),
      answer: answer_of(*error),
    },
    Degradation::Kept { path, kept } => LandingDegradation::Kept {
      path: path.to_string(),
      kept: kept.to_string(),
    },
  }
}

/// A host refusal as the reply carries it.
#[cfg(unix)]
fn answer_of(error: HostError) -> HostAnswer {
  match error {
    HostError::NotFound => HostAnswer::NotFound,
    HostError::NotDirectory => HostAnswer::NotDirectory,
    HostError::NotFile => HostAnswer::NotFile,
    HostError::StaleHandle => HostAnswer::StaleHandle,
    HostError::Unavailable(errno) => HostAnswer::Errno { errno },
  }
}

/// The proof of grant-issuer authority a `Grant` request carries (§4.13 "Grants": "the daemon verifies
/// that authority and the exact manifest hash, target identity, intended consumer, scope and validity
/// before accepting a grant"): the BLAKE3 **keyed** hash, under the issuer secret the daemon minted into
/// the anchor segment, over the exact landing being approved — its id (which names the volume, target and
/// intended consumer the presented record binds), the manifest hash the human saw, the scope and the term.
/// A human surface that maps the anchor (the `slates grant` command running as the anchor's user) computes
/// it; the daemon recomputes it. Every field the spec lists is in the hash, so a replayed proof for another
/// landing, a retargeted or modified plan (a different manifest), a widened scope or a longer term all
/// fail to verify — before anything is written. Pure, so it is unit-testable and the CLI and the daemon
/// share one definition.
pub fn grant_proof(
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  landing: u64,
  manifest: &[u8; 32],
  scope: GrantScope,
  term_ns: u64,
) -> [u8; 32] {
  /// Format: the scope's byte in the proof — the wire enum's own variant index, so the proof and the
  /// request agree without a second table.
  fn scope_byte(scope: GrantScope) -> u8 {
    match scope {
      GrantScope::Once => 0,
      GrantScope::Session => 1,
    }
  }
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(&landing.to_le_bytes());
  hasher.update(manifest);
  hasher.update(&[scope_byte(scope)]);
  hasher.update(&term_ns.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// Serves a `Grant` request (§4.13 "Grants", §4.15 step 3): verifies the proof of issuer authority against
/// the secret this daemon published, and the approved manifest against the presented landing's, then
/// issues the grant. Refuses `GrantIssuerUnverified` — counted `grant_issuer_unverified` — on a proof that
/// does not verify (a forged, replayed or modified-plan approval, or one from a channel that carries no
/// authority: the MCP server and the SDKs never hold the secret), `GrantMismatch` when the human approved
/// a manifest other than the one presented (the plan changed under the approval), and `NotFound` for a
/// landing not awaiting a grant (consumed, or never presented — a replay of a spent approval). The
/// comparison of the proof is constant-time over its whole width so a wrong proof leaks nothing by timing.
pub fn grant_verb(
  state: &mut ShardState,
  principal: &Principal,
  landing_id: u64,
  manifest: [u8; 32],
  scope: GrantScope,
  term_ns: u64,
  proof: [u8; 32],
) -> ReplyBody {
  let Some(awaiting) = state.landing.awaiting.get(&landing_id).cloned() else {
    return crate::verbs::refused(Refusal::NotFound);
  };
  let expected = grant_proof(&state.issuer_secret, landing_id, &manifest, scope, term_ns);
  if !constant_time_eq(&expected, &proof) {
    state.count("grant_issuer_unverified", 1);
    return crate::verbs::refused(Refusal::GrantIssuerUnverified);
  }
  if awaiting.manifest != manifest {
    return crate::verbs::refused(Refusal::GrantMismatch);
  }
  match issue_grant(state, principal, landing_id, scope, term_ns) {
    Ok(grant) => ReplyBody::Granted { grant },
    Err(refusal) => crate::verbs::refused(refusal),
  }
}

/// Format: the domain tags that separate the issuer secret's uses — an enrollment proof can never
/// verify as a revocation proof or a grant proof, whatever the bytes after the tag.
const ENROLL_DOMAIN: &[u8] = b"slates.enroll";
/// Format: see `ENROLL_DOMAIN`.
const REVOKE_DOMAIN: &[u8] = b"slates.revoke";

/// The proof of issuer authority an `Enroll` request carries (§4.13 "Principals": a *trusted* enrollment
/// establishes a consumer — the same human surface that issues grants): the keyed hash, under the issuer
/// secret, of the enrollment domain tag and the account the consumer is enrolled under.
pub fn enroll_proof(
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  account: u32,
) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(ENROLL_DOMAIN);
  hasher.update(&account.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// The proof of issuer authority a `Revoke` request carries: the keyed hash, under the issuer secret, of
/// the revocation domain tag and the consumer revoked.
pub fn revoke_proof(
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  consumer: u64,
) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(REVOKE_DOMAIN);
  hasher.update(&consumer.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// The proof a workload presents to bind its channel to an enrolled consumer (§4.13 "a consumer channel
/// is bound at rendezvous using a capability delivered and retained outside other agents' reach"): the
/// keyed hash, under the consumer's secret capability, of the client id the daemon assigned this channel
/// — so a proof captured from one session cannot bind another (the id differs), and the capability itself
/// never crosses the ring. Defined once, beside the delivery channel the workload takes the capability
/// from (`slates_ipc::delivery`), so the workload's proof and this daemon's check are one formula; the
/// golden vector below pins it.
pub use slates_ipc::delivery::attest_proof;

/// Whether two proofs are equal, visiting every byte whatever the first difference (no early exit), so
/// the comparison's time does not depend on how much of a forged proof happened to match.
pub(crate) fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
  let mut difference = 0u8;
  for (x, y) in a.iter().zip(b.iter()) {
    difference |= x ^ y;
  }
  difference == 0
}

/// Issues a grant for a presented landing, binding its manifest (§4.15 step 3). Reached only through
/// [`grant_verb`], whose proof of issuer authority has verified, so a grant a human did not make cannot
/// exist. The grant is issued into the runtime grants and persisted as a `GrantRecord`; the landing's
/// record gains the grant. Returns the grant id.
pub fn issue_grant(
  state: &mut ShardState,
  principal: &Principal,
  landing_id: u64,
  scope: GrantScope,
  term_ns: u64,
) -> Result<u64, Refusal> {
  let awaiting = state
    .landing
    .awaiting
    .get(&landing_id)
    .cloned()
    .ok_or(Refusal::NotFound)?;
  let now = state.clock.monotonic_ns();
  let land_scope = match scope {
    GrantScope::Once => LandScope::Once,
    GrantScope::Session => LandScope::Session,
  };
  // A grant id is never reused; once the id space is spent (2^64 grants — memory runs out first, one record
  // per grant) the grant is refused as a spent capacity. The grant binds the presented landing: its consumer,
  // volume, snapshot and target identity (AUD-29-01).
  let id = state
    .landing
    .grants
    .issue(
      Surface::Cli,
      awaiting.manifest,
      awaiting.binding.clone(),
      land_scope,
      now,
      term_ns,
    )
    .ok_or(Refusal::NoSpace)?;
  // The durable record names the principal the grant was made for — the consumer the landing was presented
  // to, and a session grant that consumer's session — not the human who issued it (the surface records
  // that); until 2026-09-29 it recorded the issuer for both.
  let record = DbGrantRecord {
    id: id.0,
    principal: awaiting.principal.clone(),
    surface: GrantSurface::Cli,
    volume: awaiting.volume,
    snapshot: awaiting.snapshot,
    target: awaiting.target.clone(),
    manifest: awaiting.manifest,
    scope: db_grant_scope(scope, session_of(&awaiting.principal)),
    issued_ns: now,
    expires_ns: now.saturating_add(term_ns),
    state: DbGrantState::Issued,
    target_device: awaiting.binding.target.device,
    target_inode: awaiting.binding.target.inode,
  };
  let audit = DbAuditRecord {
    seq: 0,
    at_ns: now,
    kind: DbAuditKind::GrantIssued,
    principal: principal.clone(),
    grant: Some(id.0),
    landing: Some(landing_id),
    manifest: Some(awaiting.manifest),
    outcome: None,
  };
  for op in [
    Op::GrantIssued { record },
    Op::AuditAppended { record: audit },
  ] {
    state
      .db
      .mutate(&mut state.segment, &op, now)
      .map_err(|e| crate::error::refusal_of_db(&e))?;
  }
  Ok(id.0)
}

/// Rebuilds the runtime grants from the durable records a restarted shard recovered (AUD-29-06): each grant in
/// its recorded state and bound as it was approved — the consumer by the principal it was made for, the
/// volume, snapshot and target by their recorded identities — so a session grant stays usable for exactly its
/// binding across a restart, a spent or revoked one stays spent, and the next grant takes an id past every
/// recorded one. Before 2026-09-29 the runtime table started empty: every grant was lost at a restart and the
/// first grant after it re-minted a recorded id, refused `AlreadyExists`.
pub fn restore_grants(state: &mut ShardState) {
  let records: Vec<DbGrantRecord> = state.db.partition().grants().cloned().collect();
  for record in records {
    state.landing.grants.restore(LandGrantRecord {
      id: slates_land::grant::GrantId(record.id),
      surface: match record.surface {
        GrantSurface::Cli => Surface::Cli,
        GrantSurface::Confirmation { .. } => Surface::Confirmation,
      },
      manifest: record.manifest,
      binding: GrantBinding {
        consumer: record.principal.key().into_boxed_slice(),
        volume: record.volume.bytes,
        snapshot: record.snapshot.value,
        target: TargetIdentity {
          key: record.target.as_str().into(),
          device: record.target_device,
          inode: record.target_inode,
        },
      },
      scope: match record.scope {
        DbGrantScope::Once => LandScope::Once,
        DbGrantScope::Session { .. } => LandScope::Session,
      },
      issued_ns: record.issued_ns,
      expires_ns: record.expires_ns,
      state: match record.state {
        DbGrantState::Issued => LandGrantState::Issued,
        DbGrantState::Consumed => LandGrantState::Consumed,
        DbGrantState::Expired => LandGrantState::Expired,
        DbGrantState::Revoked => LandGrantState::Revoked,
      },
    });
  }
}

/// The caller's grants, from the durable records.
pub fn grants_verb(state: &ShardState, principal: &Principal) -> ReplyBody {
  let grants = state
    .db
    .partition()
    .grants_of(principal)
    .into_iter()
    .map(|g| GrantSummary {
      id: g.id,
      volume: to_wire_volume(g.volume),
      target: g.target.clone(),
      manifest: g.manifest,
      scope: wire_grant_scope(&g.scope),
      state: grant_state_name(g.state),
    })
    .collect();
  ReplyBody::Grants { grants }
}

/// The audit log from `since`, from the durable records.
pub fn audit_verb(state: &ShardState, since: u64) -> ReplyBody {
  let records = state
    .db
    .partition()
    .audit()
    .iter()
    .filter(|r| r.seq >= since)
    .map(|r| AuditEntry {
      seq: r.seq,
      at_ns: r.at_ns,
      kind: audit_kind_name(r.kind),
      grant: r.grant,
      landing: r.landing,
      manifest: r.manifest,
      outcome: r.outcome.map(landing_state_name),
    })
    .collect();
  ReplyBody::Audit { records }
}

fn to_wire_volume(id: DbVolumeId) -> VolumeId {
  VolumeId { bytes: id.bytes }
}

fn session_of(principal: &Principal) -> u64 {
  match principal {
    Principal::Uid { uid } => u64::from(*uid),
    // A consumer's session is its own, not its account's: two consumers under one uid never share a
    // session-scoped grant (§4.13 "distinct consumers sharing a uid cannot use each other's grant rights").
    Principal::Consumer { consumer, .. } => *consumer,
    Principal::Sid { .. } | Principal::Certificate { .. } => 0,
  }
}

#[cfg(unix)]
fn to_land_filter(filter: &slates_ipc::protocol::Filter) -> Filter {
  Filter {
    include: filter.include.iter().map(|s| s.as_str().into()).collect(),
    exclude: filter.exclude.iter().map(|s| s.as_str().into()).collect(),
  }
}

#[cfg(unix)]
fn summary_of(manifest: &Manifest) -> LandingSummary {
  LandingSummary {
    by_action: manifest
      .summary
      .by_action
      .iter()
      .map(|(action, count)| ActionCount {
        action: action.to_string(),
        count: *count as u64,
      })
      .collect(),
    bytes: manifest.summary.bytes,
    filtered_out: manifest.summary.filtered_out as u64,
  }
}

#[cfg(unix)]
fn target_refusal(refusal: &TargetRefusal) -> Refusal {
  Refusal::TargetUnavailable {
    reason: match refusal {
      TargetRefusal::NotAbsolute => "the target path is not absolute".to_owned(),
      TargetRefusal::EscapesTarget => "a component escapes the target directory".to_owned(),
      TargetRefusal::TargetNotOwned => "the target is owned by another user".to_owned(),
      TargetRefusal::Unavailable(e) => format!("{e:?}"),
    },
  }
}

#[cfg(unix)]
fn db_landing_state(state: &slates_land::engine::LandingState) -> DbLandingState {
  use slates_land::engine::LandingState as L;
  match state {
    L::Planning => DbLandingState::Planning,
    L::AwaitingGrant => DbLandingState::AwaitingGrant,
    L::Validating => DbLandingState::Validating,
    L::Writing => DbLandingState::Writing,
    L::Syncing => DbLandingState::Syncing,
    L::Advancing => DbLandingState::Advancing,
    L::Done => DbLandingState::Done,
    L::Partial => DbLandingState::Partial,
    L::Refused => DbLandingState::Refused,
    L::Aborted => DbLandingState::Aborted,
  }
}

fn landing_state_name(state: DbLandingState) -> String {
  match state {
    DbLandingState::Planning => "planning",
    DbLandingState::AwaitingGrant => "awaiting_grant",
    DbLandingState::Validating => "validating",
    DbLandingState::Writing => "writing",
    DbLandingState::Syncing => "syncing",
    DbLandingState::Advancing => "advancing",
    DbLandingState::Done => "done",
    DbLandingState::Partial => "partial",
    DbLandingState::Refused => "refused",
    DbLandingState::Aborted => "aborted",
  }
  .to_owned()
}

#[cfg(unix)]
fn db_audit(audit: &AuditRecord, principal: &Principal) -> DbAuditRecord {
  DbAuditRecord {
    seq: 0,
    at_ns: audit.at_ns,
    kind: db_audit_kind(audit.kind),
    principal: principal.clone(),
    grant: audit.grant.map(|g| g.0),
    landing: Some(audit.landing),
    manifest: Some(audit.manifest),
    outcome: audit.outcome.map(|s| db_landing_state(&s)),
  }
}

#[cfg(unix)]
fn db_audit_kind(kind: AuditKind) -> DbAuditKind {
  match kind {
    AuditKind::LandingPlanned => DbAuditKind::LandingPlanned,
    AuditKind::LandingValidated => DbAuditKind::LandingValidated,
    AuditKind::EntryWritten => DbAuditKind::EntryWritten,
    AuditKind::EntryRefused => DbAuditKind::EntryRefused,
    AuditKind::LandingFinished => DbAuditKind::LandingFinished,
  }
}

fn audit_kind_name(kind: DbAuditKind) -> String {
  match kind {
    DbAuditKind::GrantIssued => "grant_issued",
    DbAuditKind::GrantRevoked => "grant_revoked",
    DbAuditKind::LandingPlanned => "landing_planned",
    DbAuditKind::LandingValidated => "landing_validated",
    DbAuditKind::EntryWritten => "entry_written",
    DbAuditKind::EntryRefused => "entry_refused",
    DbAuditKind::LandingFinished => "landing_finished",
  }
  .to_owned()
}

fn db_grant_scope(scope: GrantScope, session: u64) -> DbGrantScope {
  match scope {
    GrantScope::Once => DbGrantScope::Once,
    GrantScope::Session => DbGrantScope::Session { session },
  }
}

fn wire_grant_scope(scope: &DbGrantScope) -> GrantScope {
  match scope {
    DbGrantScope::Once => GrantScope::Once,
    DbGrantScope::Session { .. } => GrantScope::Session,
  }
}

fn grant_state_name(state: DbGrantState) -> String {
  match state {
    DbGrantState::Issued => "issued",
    DbGrantState::Consumed => "consumed",
    DbGrantState::Expired => "expired",
    DbGrantState::Revoked => "revoked",
  }
  .to_owned()
}

#[cfg(all(test, unix))]
mod tests {
  use super::{
    ENROLL_DOMAIN, REVOKE_DOMAIN, SpanObserver, attest_proof, enroll_proof, release_target_lease,
    revoke_proof, take_target_lease,
  };
  use slates_ipc::protocol::Refusal;
  use slates_vfs::clock::Clock;

  /// Shape: a lease term longer than any of these histories, in nanoseconds (a minute).
  const LONG_TERM_NS: u64 = 60_000_000_000;
  /// Shape: a lease term the histories outlive by pausing, in nanoseconds (20 ms).
  const SHORT_TERM_NS: u64 = 20_000_000;
  /// Shape: how many targets the bound history leases at once.
  const TARGETS: u64 = 5;

  /// The canonical key of a target, as `slates_land::grant::lease_key` spells one (device 1, `inode`).
  fn target(inode: u64) -> String {
    slates_land::grant::lease_key(&slates_land::grant::TargetIdentity {
      key: "/".into(),
      device: 1,
      inode,
    })
  }

  /// A deadline the history never reaches.
  fn far(state: &mut crate::state::ShardState) -> u64 {
    state.clock.monotonic_ns().saturating_add(LONG_TERM_NS)
  }

  /// Outlives a short term: the paused holder.
  fn pause_past_a_short_term() {
    // The paused holder: the test stands in for a descheduled landing attempt.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_nanos(2 * SHORT_TERM_NS));
  }

  /// §4.15 step 4 (AUD-29-03): the control shard's target lease has one holder until it is released or its
  /// term ends. Do: attempt A takes a target; attempts B and A again take it; B releases it; A releases it
  /// and C takes it. Expect: B's take and A's second take are refused `LandingLeaseHeld` naming A — an attempt
  /// id is not a renewal; another target is its own lease; B's release frees nothing; after A's release C
  /// holds it under a greater generation; the status report counts each record and none once released.
  #[test]
  fn a_target_lease_has_one_holder_until_its_release_or_its_term() {
    crate::daemon::audit_on_shard(|state| {
      let deadline = far(state);
      let a = take_target_lease(state, target(2), 10, LONG_TERM_NS, deadline).unwrap();
      for holder in [11, 10] {
        assert_eq!(
          take_target_lease(state, target(2), holder, LONG_TERM_NS, deadline),
          Err(Refusal::LandingLeaseHeld { holder: 10 }),
          "attempt {holder}"
        );
      }
      take_target_lease(state, target(3), 11, LONG_TERM_NS, deadline).unwrap();
      assert_eq!(crate::verbs::shard_report(state).target_leases, 2);
      release_target_lease(state, &target(2), 11).unwrap();
      assert_eq!(
        take_target_lease(state, target(2), 12, LONG_TERM_NS, deadline),
        Err(Refusal::LandingLeaseHeld { holder: 10 }),
        "another attempt's release freed nothing"
      );
      release_target_lease(state, &target(2), 10).unwrap();
      let c = take_target_lease(state, target(2), 12, LONG_TERM_NS, deadline).unwrap();
      assert!(c.generation > a.generation, "{c:?} after {a:?}");
      release_target_lease(state, &target(2), 12).unwrap();
      release_target_lease(state, &target(3), 11).unwrap();
      assert_eq!(crate::verbs::shard_report(state).target_leases, 0);
    });
  }

  /// §4.15 step 4 (AUD-29-03, a paused old holder): a lease whose term ends is overtaken, and its holder's
  /// late release frees nothing of the new holder's. Do: attempt A takes a target for a short term and
  /// pauses past it; B takes the target; A releases. Expect: B's take succeeds under a greater generation,
  /// and after A's release the target is still B's.
  #[test]
  fn a_holder_paused_past_its_term_is_overtaken_and_its_release_frees_nothing() {
    crate::daemon::audit_on_shard(|state| {
      let deadline = far(state);
      let a = take_target_lease(state, target(2), 10, SHORT_TERM_NS, deadline).unwrap();
      pause_past_a_short_term();
      let b = take_target_lease(state, target(2), 11, LONG_TERM_NS, deadline).unwrap();
      assert!(b.generation > a.generation, "{b:?} after {a:?}");
      release_target_lease(state, &target(2), 10).unwrap();
      assert_eq!(
        take_target_lease(state, target(2), 12, LONG_TERM_NS, deadline),
        Err(Refusal::LandingLeaseHeld { holder: 11 }),
        "the paused holder's release freed nothing of the new holder's"
      );
    });
  }

  /// §4.15 step 4 (AUD-29-03): a take that reaches the control shard after its caller stopped waiting takes
  /// nothing, so no lease is held for an attempt that never learns of it. Do: take with a deadline already
  /// passed, then take for another attempt. Expect: the first is refused `LandingLeaseLost` and the second
  /// holds the target at once.
  #[test]
  fn a_take_past_its_callers_deadline_takes_nothing() {
    crate::daemon::audit_on_shard(|state| {
      let passed = state.clock.monotonic_ns();
      assert_eq!(
        take_target_lease(state, target(2), 10, LONG_TERM_NS, passed),
        Err(Refusal::LandingLeaseLost)
      );
      let deadline = far(state);
      let taken = take_target_lease(state, target(2), 11, LONG_TERM_NS, deadline).unwrap();
      assert_eq!(taken.holder, 11);
    });
  }

  /// §4.15 step 4, ban 8 (AUD-29-03): the control shard holds no more lease records than the leases live at
  /// its last take — a lease whose release never came is released by the next take once its term ends. Do:
  /// lease five targets for a short term, releasing none; pause past the term; lease a sixth. Expect: the
  /// status report counts five, then one.
  #[test]
  fn a_take_releases_the_leases_whose_terms_have_ended() {
    crate::daemon::audit_on_shard(|state| {
      let deadline = far(state);
      for inode in 0..TARGETS {
        take_target_lease(state, target(inode), inode, SHORT_TERM_NS, deadline).unwrap();
      }
      assert_eq!(crate::verbs::shard_report(state).target_leases, TARGETS);
      pause_past_a_short_term();
      take_target_lease(state, target(TARGETS), TARGETS, LONG_TERM_NS, deadline).unwrap();
      assert_eq!(crate::verbs::shard_report(state).target_leases, 1);
    });
  }

  /// The landing span observer is bounded shed-first (§4.14): recording more entry timings than its
  /// capacity keeps the most recent and counts the shed ones, so a large landing never grows the buffer
  /// without bound. Do: record five timings into a capacity of three. Expect: three held (the most
  /// recent, oldest first) and two counted as dropped — the loss the server folds into the sink's total.
  #[test]
  fn the_landing_observer_is_bounded_and_counts_shed_timings() {
    let mut observer = SpanObserver::with_capacity(3);
    for i in 0..5u64 {
      observer.record(i, i + 1);
    }
    assert_eq!(observer.entries.len(), 3, "held at the bound");
    assert_eq!(observer.dropped, 2, "the two oldest were shed and counted");
    let held: Vec<u64> = observer.entries.iter().map(|(start, _)| *start).collect();
    assert_eq!(
      held,
      vec![2, 3, 4],
      "the three most recent survived, oldest first"
    );
  }

  /// §4.13 "Grants" / "Principals": the three proofs of the enrollment surface are keyed BLAKE3 over
  /// **domain-separated** inputs, so no proof of one kind verifies as another — an enrollment approval
  /// can never be replayed as a revocation, nor a revocation as an enrollment of the same number — and an
  /// attestation is bound to the channel it was made for. Golden vectors pin the exact bytes: a refactor
  /// that dropped a domain tag, reordered a field, or changed the key would change them.
  #[test]
  fn the_enrollment_proofs_are_domain_separated_and_pinned() {
    let issuer = [0x11u8; slates_anchor::layout::ISSUER_SECRET_BYTES];
    let capability = [0x22u8; 32];
    // The same number as an account and as a consumer id: the domain tag alone must separate them.
    const NUMBER: u64 = 7;
    #[allow(clippy::cast_possible_truncation)]
    let account = NUMBER as u32;

    let enroll = enroll_proof(&issuer, account);
    let revoke = revoke_proof(&issuer, NUMBER);
    assert_ne!(
      enroll, revoke,
      "an enrollment proof is never a revocation proof"
    );
    assert_ne!(
      enroll_proof(&issuer, account),
      enroll_proof(
        &[0x12u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
        account
      ),
      "the proof is keyed: another issuer secret proves nothing"
    );
    assert_ne!(
      attest_proof(&capability, 1),
      attest_proof(&capability, 2),
      "an attestation is bound to its channel's client id"
    );
    assert_ne!(
      attest_proof(&capability, 1),
      attest_proof(&[0x23u8; 32], 1),
      "an attestation is keyed by the capability"
    );

    // Golden vectors (BLAKE3 keyed; recomputed by the same law, so a change here is a wire change).
    let hex = |bytes: &[u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let expect_enroll = {
      let mut h = blake3::Hasher::new_keyed(&issuer);
      h.update(ENROLL_DOMAIN);
      h.update(&account.to_le_bytes());
      *h.finalize().as_bytes()
    };
    let expect_revoke = {
      let mut h = blake3::Hasher::new_keyed(&issuer);
      h.update(REVOKE_DOMAIN);
      h.update(&NUMBER.to_le_bytes());
      *h.finalize().as_bytes()
    };
    let expect_attest = {
      let mut h = blake3::Hasher::new_keyed(&capability);
      h.update(&1u32.to_le_bytes());
      *h.finalize().as_bytes()
    };
    assert_eq!(hex(&enroll), hex(&expect_enroll));
    assert_eq!(hex(&revoke), hex(&expect_revoke));
    assert_eq!(hex(&attest_proof(&capability, 1)), hex(&expect_attest));
    assert_ne!(
      hex(&enroll),
      hex(&{
        // The tag is what separates: the same bytes with the tags swapped are a different proof.
        let mut h = blake3::Hasher::new_keyed(&issuer);
        h.update(REVOKE_DOMAIN);
        h.update(&account.to_le_bytes());
        *h.finalize().as_bytes()
      }),
      "the domain tag is part of the proof"
    );
  }
}
