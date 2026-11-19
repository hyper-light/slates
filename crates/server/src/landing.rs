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
//! write path runs on Linux, macOS and Windows (A-185: `slates-land`'s Windows seam, the same surface over NTFS
//! handles); its tests land into a real directory on the host's disk in the build output (`CARGO_TARGET_TMPDIR`; A-50:
//! never `/tmp`, never a RAM directory), on every host, exactly as the Phase 1 landing tests do.

use slates_db::Op;
use slates_db::catalog::LandingRecord;
use slates_db::catalog::{
  AuditKind as DbAuditKind, AuditRecord as DbAuditRecord, GrantRecord as DbGrantRecord,
  GrantScope as DbGrantScope, GrantState as DbGrantState, GrantSurface,
  LandingState as DbLandingState, Principal, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId,
};
use slates_ipc::protocol::{
  ActionCount, HostAnswer, LandingDegradation, LandingDurability, LandingOutcome, LandingSummary,
  WriteAct,
};
use slates_ipc::protocol::{
  AuditEntry, GrantScope, GrantSummary, PolicySubject, Refusal, ReplyBody, SnapshotId, VolumeId,
  WriteMode, WritePolicy,
};
use slates_land::engine::Audit;
use slates_land::engine::{AuditKind, AuditRecord};
use slates_land::engine::{
  Degradation, Durability, HostSlot, LandingRefusal, LandingReport, LandingRequest, LandingRun,
  Observer, settle_grant,
};
use slates_land::grant::{
  GrantBinding, GrantRecord as LandGrantRecord, GrantScope as LandScope,
  GrantState as LandGrantState, Grants, Surface, TargetIdentity,
};
use slates_land::grant::{GrantId, GrantRefusal, LandingLease};
use slates_land::manifest::{Filter, LandingEntry, Manifest};
use slates_land::os::{Away, OsLand, TargetRefusal};
use slates_land::source::Source;
use slates_vfs::clock::Clock;
use slates_vfs::host::{Barrier, HostError, HostFs, LandFs};
use slates_wire::observe::Chokepoint;

use crate::error::refusal_of_vfs;
use crate::state::ShardState;
use crate::verbs::refused;
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
  /// Test support: while set, this shard sends the control partition none of what it owes the audit (A-265's crossing;
  /// `Daemon::withhold_audit_crossing`), so a test holds a record between its atom's commit and its batch, where a
  /// crash used to lose it. Never set from the wire.
  pub crossing_withheld: bool,
  /// Test support: while set, this shard records no acknowledgement of its appended audit records (A-265;
  /// `Daemon::withhold_audit_acknowledgement`), as if each were lost, so its outbox keeps them and the deadline
  /// keeper's look sends them again. Never set from the wire.
  pub acknowledgement_withheld: bool,
  /// The crossing sequence at the front of this partition's outbox when its look was armed or last taken (A-265,
  /// A-554): a front that has not moved by the look, a retry window on, is sent again ([`look_at_owed_audit`]), so a
  /// batch lost to a crash, a restart or an unanswered call reaches the control partition. `None` while no look is
  /// armed.
  pub outbox_front_seen: Option<u64>,
  /// When the shard's deadline keeper looks next at what this partition owes the audit (A-554): a retry window after
  /// the record that armed it was owed, then a window after each look while any is owed ([`arm_owed_audit_look`]);
  /// `None` while nothing is owed. Until A-554 the reap loop's tick looked once a window whether anything was owed or
  /// not.
  pub audit_look_at: Option<u64>,
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
  /// The longest a granted landing on this shard went between two looks at its lease, in nanoseconds: from the
  /// take to the first look, then from look to look (`keep_lease_alive`). The keepalive renews once less than half
  /// the term is left (`RENEW_AT_REMAINING_PERMILLE`), so a gap past half the term lapses the lease and the entries
  /// after it are skipped `LeaseEnded`, as a holder paused past its term must be (AUD-29-03). The measure a term is
  /// judged against: a busy machine's gaps, not a guess.
  pub longest_lease_gap_ns: u64,
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
  /// The longest gap between two of a landing's looks at its lease ([`LandingState::longest_lease_gap_ns`]).
  pub longest_lease_gap_ns: u64,
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
      longest_lease_gap_ns: self.longest_lease_gap_ns,
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
  /// Its manifest's entries, in the manifest's order: the full entry list a person may ask for (§4.15 step 2;
  /// `LandingEntries` pages them, `crate::landing_entries`). Moved from the plan, never copied, and held as long as the
  /// presentation is, which A-46 bounds: consumed by its landing, replaced by its client's next, abandoned with its
  /// client, at most one a client seat. Their size is the plan's, which the volume's diverged set bounds.
  pub entries: Vec<slates_land::manifest::LandingEntry>,
  /// The conflict classes the preliminary verdict pass found among them, by path (the conflicting entries alone).
  pub conflicts: std::collections::BTreeMap<Box<str>, slates_land::verdict::ConflictClass>,
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
      crossing_withheld: false,
      acknowledgement_withheld: false,
      outbox_front_seen: None,
      audit_look_at: None,
      next_landing: 1,
      awaiting: std::collections::BTreeMap::new(),
      in_flight: std::collections::BTreeMap::new(),
      next_holder: 1,
      running: std::collections::BTreeMap::new(),
      slice_times: crate::histogram::DurationHistogram::default(),
      slices_past_budget: 0,
      slice_budget_ns: 0,
      longest_lease_gap_ns: 0,
    }
  }
}

/// The identity of a landing: its id and what it lands where.
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
  /// Present only (`Present`, A-285): answer the presentation whatever the write policy, never land.
  pub present_only: bool,
  /// A signature over the landing's statement (signed work, A-321): judged against the manifest presented or landed,
  /// recorded with the landing's audit records and answered; it permits nothing.
  pub signature: Option<&'a [u8]>,
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
  land_verb_os(state, client_id, principal, call)
}

/// Takes the landing lease on the target whose canonical key is `key`, on this shard — the control shard,
/// the one owner of the host's target leases (§4.15 "Ownership facts", step 4; AUD-29-03) — for the landing
/// attempt `holder`, for `term_ns`, as a durable record. Refused `LandingLeaseHeld`, naming the holder,
/// while another attempt's lease on that target is unexpired; the holder's own take renews it (the keepalive,
/// AUD-29-25). Refused `LandingLeaseLost`, taking nothing, once
/// the caller's `deadline_ns` has passed: the caller no longer waits for the answer, and a lease it never
/// learned of would hold the target for a term. The generation is the take's log sequence on this
/// partition, so it only grows, across releases and restarts: a release or a fence naming an older one
/// names a lease that is gone.
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
const LEASE_TAKE_LATE: &str = "landing.lease_take_late";

/// Releases every target lease whose term has ended, so the control partition holds no more records than
/// the leases live at its last take (ban 8): a lease whose release never came — its attempt's compensation
/// lost as well, or its daemon gone — would otherwise stay for good. Bounded by those records.
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
struct SpanObserver {
  entries: std::collections::VecDeque<(u64, u64)>,
  capacity: usize,
  dropped: u64,
}

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

/// The landing: open the target through `OsLand` (the Unix seam, or the Windows one since A-185) and run the engine.
fn land_verb_os(
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
    present_only,
    signature,
  } = call;
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("land");
  }
  // The write policy (A-285): a forbidding mode refuses every landing, granted or not, before any host access; a
  // bypass mode grants the landing the daemon presents and lands it in the same request.
  let mode = land_mode(&record);
  if mode == slates_db::catalog::WriteMode::Forbid && !present_only {
    state.count(POLICY_FORBIDS_LAND, 1);
    return refused(Refusal::PolicyForbids {
      act: WriteAct::Land,
      by: PolicySubject::Volume { volume },
    });
  }
  let bypass = !present_only && grant.is_none() && mode == slates_db::catalog::WriteMode::Bypass;
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
    Err(refusal) => return refused(target_refusal(&refusal, target)),
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
    bypass: bypass.then(|| Requested {
      volume,
      snapshot,
      filter: filter.clone(),
    }),
    signature: signature.map(<[u8]>::to_vec),
  };
  match grant {
    // A presentation writes nothing and takes no lease; it is planned in slices and answered when planned.
    None => defer_presentation(state, prepared),
    // A granted landing runs as an owned task under its target's lease (AUD-29-03).
    Some(_) => defer_granted_landing(state, prepared),
  }
}

/// Format: the counter of landings a forbidding write policy refused (A-285).
const POLICY_FORBIDS_LAND: &str = "policy_forbids.land";
/// Format: the counter of landings a bypass write policy granted (A-285).
const POLICY_GRANTED_LAND: &str = "policy_granted.land";

/// What a landing request named, kept by a bypass presentation (A-285) to land it under the grant the policy issues:
/// the request's own volume, snapshot (the head when `None`) and filter.
struct Requested {
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  filter: slates_ipc::protocol::Filter,
}

/// A landing ready to run: its volume, its opened target and what its request and records name.
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
  /// What to land under the grant a bypass policy issues once the presentation is planned (A-285); `None` for a
  /// presentation that waits for an approval, and for a granted landing.
  bypass: Option<Requested>,
  /// The landing's signature, if the request carried one (A-321).
  signature: Option<Vec<u8>>,
}

/// What a captured clone is landed against (§4.15 `capture_base`, A-291's landing): on the node that made it, its
/// captured snapshot's tree, when `volume` is a scratch clone of a snapshot recorded captured (`Op::BaseCaptured`) whose
/// volume this shard serves (stage 1); on a node a move brought it to, what the move carried of the capture and the
/// snapshot here holding the head it handed over, its mark (stage 2, A-503); `None` for any other volume, which lands
/// as before. A clone and its origin share one store (D-5), so a captured clone's capture is on its own shard; a capture
/// it names that is not served here (its origin left unrecovered) is refused, never landed as a scratch volume, which
/// would plan every file as a create and carry no removal; and so is a carried capture whose mark this node does not
/// hold (a takeover that rebuilt a later head than the moved one, or the mark's snapshot destroyed), or whose carried
/// paths do not parse. Also what a move from this node carries again ([`crate::moves`]).
pub(crate) fn capture_of(
  state: &ShardState,
  volume: DbVolumeId,
) -> Result<Option<slates_vfs::captured::CaptureBase>, Refusal> {
  let partition = state.db.partition();
  let scratch = partition
    .volume(volume)
    .is_some_and(|record| matches!(record.base, slates_db::catalog::BaseRecord::Scratch));
  if !scratch {
    return Ok(None);
  }
  if let Some(edge) = partition.lineage(volume)
    && partition.is_captured(edge.origin_volume, edge.origin_snapshot)
  {
    let snapshot = crate::verbs::core_snapshot(SnapshotId {
      value: edge.origin_snapshot.value,
    });
    let Some(origin) = state
      .by_id
      .get(&edge.origin_volume)
      .and_then(|handle| state.volumes.get(*handle).ok())
    else {
      return Err(Refusal::Unsupported {
        feature: "landing a captured clone whose capture this node does not serve".to_owned(),
      });
    };
    return origin
      .volume
      .frozen_tree(snapshot)
      .map(|tree| Some(tree.into()))
      .map_err(|error| refusal_of_vfs(&error));
  }
  let Some((carried, mark)) = partition.carried(volume) else {
    return Ok(None);
  };
  let Some(mark) = mark else {
    return Err(Refusal::Unsupported {
      feature: "landing a moved captured clone whose moved head this node does not hold".to_owned(),
    });
  };
  let carried = slates_vfs::captured::CarriedCapture::parse(&carried.entries)
    .map_err(|error| refusal_of_vfs(&error))?;
  let Some(slot) = state
    .by_id
    .get(&volume)
    .and_then(|handle| state.volumes.get(*handle).ok())
  else {
    return Err(Refusal::NotFound);
  };
  let mark = slot
    .volume
    .frozen_tree(crate::verbs::core_snapshot(SnapshotId {
      value: mark.value,
    }))
    .map_err(|error| refusal_of_vfs(&error))?;
  Ok(Some(slates_vfs::captured::CaptureBase::Carried {
    mark,
    carried,
  }))
}

/// A landing's beginning: against its capture for a captured clone ([`capture_of`]), else from the volume's own
/// diverged entries.
fn landing_beginning(
  vol: &slates_vfs::volume::Volume,
  target: &slates_land::engine::LandingTarget,
  request: &LandingRequest,
  against: Option<slates_vfs::captured::CaptureBase>,
) -> Result<slates_land::engine::Beginning, slates_land::engine::LandingRefusal> {
  match against {
    Some(base) => slates_land::engine::Beginning::against_capture(vol, target, request, base),
    None => slates_land::engine::Beginning::new(vol, target, request),
  }
}

/// A presentation (§4.15 steps 1–3; AUD-29-07): the landing planned and its entries checked against the target without
/// a write, answered `GrantRequired` with what a human grants. Planned in a task of the volume's owner shard a unit at a
/// time (`slates_land::engine::Beginning` through `image::sliced`, §4.3 "Bounded work everywhere"), its reply deferred
/// to it, as a pin's and a green's creation are; an unnamed landing plans an implicit snapshot of the head, so writes
/// between its slices cannot tear its manifest, and the snapshot is dropped when it is answered. It was one call: 1,000
/// files of 16 KiB held the shard for one poll of 6.77 s on a Mac, 6.20 s of it the plan, and every verb on the shard
/// waited for it (`crates/server/tests/presentation_fairness.rs`).
fn defer_presentation(state: &mut ShardState, prepared: Prepared) -> ReplyBody {
  let Some(request_id) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a presentation outside a recorded request".to_owned(),
    });
  };
  // The id names its owner partition (`verbs::landing_id`), so the `Grant` that later covers it routes to this shard,
  // where the presented record waits.
  let landing_id = crate::verbs::landing_id(state.partition, state.landing.next_landing);
  let against = match capture_of(state, prepared.volume) {
    Ok(against) => against,
    Err(refusal) => return refused(refusal),
  };
  let Ok(slot) = state.volumes.get_mut(prepared.handle) else {
    return refused(Refusal::NotFound);
  };
  let (source, implicit) = match prepared.source {
    Source::Head => match slot.volume.snapshot(&mut state.store) {
      Ok(id) => (Source::Snapshot(id), Some(id)),
      Err(e) => {
        state.count(LANDING_SNAPSHOT_REFUSED, 1);
        return refused(refusal_of_vfs(&e));
      }
    },
    named => (named, None),
  };
  let was_overlay = slot.volume.is_overlay();
  // One host per volume: an overlay's base names its directories by the handles of the host its slot holds, so the
  // writer is built inside that host, with the target walked again under the same containment, and the host goes back
  // to the slot between units.
  let (os, land_target) = if was_overlay {
    let Some(host) = slot.host.take() else {
      drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
      return refused(Refusal::BaseUnavailable {
        path: prepared.target.clone(),
        errno: 0,
      });
    };
    match OsLand::open_target_in(host, std::path::Path::new(&prepared.target)) {
      Ok(pair) => pair,
      Err((host, refusal)) => {
        slot.host = Some(host);
        drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
        return refused(target_refusal(&refusal, &prepared.target));
      }
    }
  } else {
    (prepared.os, prepared.land_target)
  };
  let request = LandingRequest {
    landing_id,
    // The grant binds the caller by its exact principal (its key), never by the session number several
    // principals can share, and the volume and snapshot this landing names (AUD-29-01).
    consumer: prepared.principal.key().into_boxed_slice(),
    volume: prepared.volume.bytes,
    snapshot: prepared.snapshot.value,
    source,
    grant: None,
    filter: prepared.filter.clone(),
    // Read as the beginning starts: its clock counts from `Beginning::new`.
    now_ns: state.clock.monotonic_ns(),
    media_durability: false,
    large_class_bytes: state.config.large_class_bytes,
  };
  let beginning = match landing_beginning(&slot.volume, &land_target, &request, against) {
    Ok(beginning) => beginning,
    Err(refusal) => {
      give_host_back(slot, HostSlot::Owned(os), was_overlay, land_target.dir);
      drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
      let ids = LandingIds {
        landing_id,
        fresh: true,
        volume: prepared.volume,
        snapshot: prepared.snapshot,
        target: &prepared.target,
        rebased: false,
      };
      let presenter = Presenter {
        client: prepared.client,
        principal: &prepared.principal,
        signature: prepared.signature.as_deref(),
      };
      return not_landed(state, presenter, &ids, refusal);
    }
  };
  let writer = if was_overlay {
    let (host, away) = os.lend();
    slot.host = Some(host);
    Writer::Lent(away)
  } else {
    Writer::Own(os)
  };
  let job = PresentJob {
    request_id,
    handle: prepared.handle,
    landing_id,
    volume: prepared.volume,
    snapshot: prepared.snapshot,
    target: prepared.target,
    principal: prepared.principal,
    client: prepared.client,
    target_dir: land_target.dir,
    implicit,
    writer,
    beginning,
    bypass: prepared.bypass,
    signature: prepared.signature,
  };
  crate::image::defer(state, run_presentation(job))
}

/// A presentation's writer between its units: its own (a scratch volume's), or, an overlay's, lent back to the
/// volume's slot with what the writer keeps of its own.
enum Writer {
  Own(OsLand),
  Lent(slates_land::os::Away),
  /// Taken for a unit.
  Out,
}

/// A presentation between its units ([`defer_presentation`]).
struct PresentJob {
  request_id: (u64, slates_wire::request::RequestId),
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  landing_id: u64,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: String,
  principal: Principal,
  client: u32,
  target_dir: slates_vfs::host::HostDir,
  implicit: Option<slates_vfs::ids::SnapshotId>,
  writer: Writer,
  beginning: slates_land::engine::Beginning,
  bypass: Option<Requested>,
  /// The presentation's signature, if the request carried one (A-321).
  signature: Option<Vec<u8>>,
}

impl PresentJob {
  /// One unit of the presentation's beginning ([`step_beginning`]); its end once it has one.
  fn step(
    &mut self,
    state: &mut ShardState,
  ) -> Option<Result<slates_land::engine::Planned, LandingRefusal>> {
    step_beginning(state, self.handle, &mut self.writer, &mut self.beginning)
  }

  /// After the last unit, whatever its end: the overlay's walked target closed in the host its slot keeps (a scratch
  /// volume's own writer closes with it), and the implicit snapshot dropped.
  fn settle(&mut self, state: &mut ShardState) {
    let Ok(slot) = state.volumes.get_mut(self.handle) else {
      return;
    };
    if let (Writer::Lent(_), Some(host)) = (&self.writer, slot.host.as_mut()) {
      host.close_dir(self.target_dir);
    }
    self.writer = Writer::Out;
    drop_implicit(slot, &mut state.store, self.implicit, &mut state.refusals);
  }
}

/// Plans the presentation a slice at a time, then records it with the request's completion as one atom and delivers
/// the reply. With the shard's state unreachable (its shutdown) no completion is recorded, so the client's retry
/// presents again on the shard that succeeds it.
async fn run_presentation(mut job: PresentJob) {
  let ended = loop {
    match crate::image::with(|s| crate::image::sliced(s, |s| Ok(job.step(s)))) {
      Ok(Some(ended)) => break Some(ended),
      Ok(None) => slates_rt::futures::yield_now().await,
      Err(_) => break None,
    }
  };
  let request = job.request_id;
  let Some(Some((reply, route, (shard, budget)))) = crate::state::with_state(|s| {
    job.settle(s);
    // A bypass policy (A-285): a planned presentation is granted by the policy and landed in this same request. Its
    // reply, when it is not the landing's to give, goes out as a presentation's does: after its audit (A-265).
    let ended = match (job.bypass.take(), ended) {
      (Some(requested), Some(Ok(slates_land::engine::Planned::Presented(plan)))) => {
        let (reply, route) = land_under_policy(s, &job, request, requested, plan)?;
        return Some((reply, route, (s.shard, s.failover_ns)));
      }
      (_, ended) => ended,
    };
    let (reply, route) = crate::image::complete(s, request, |s| {
      let ids = LandingIds {
        landing_id: job.landing_id,
        fresh: true,
        volume: job.volume,
        snapshot: job.snapshot,
        target: &job.target,
        rebased: false,
      };
      let presenter = Presenter {
        client: job.client,
        principal: &job.principal,
        signature: job.signature.as_deref(),
      };
      match ended {
        Some(Ok(slates_land::engine::Planned::Presented(plan))) => {
          present(s, presenter, &ids, plan)
        }
        // A presentation carries no grant, so the engine never finds one: a defect, refused typed.
        Some(Ok(slates_land::engine::Planned::Granted(_))) => refused(Refusal::Unsupported {
          feature: "a granted plan in a presentation".to_owned(),
        }),
        Some(Err(refusal)) => not_landed(s, presenter, &ids, refusal),
        None => refused(Refusal::NotFound),
      }
    });
    Some((reply, route, (s.shard, s.failover_ns)))
  }) else {
    return;
  };
  // The presentation's record reaches the audit before its reply does (A-265): what the shard owes is sent and waited
  // for, and if the wait runs out the record stays owed, durable, for the next send.
  append_owed_audit(shard, budget).await;
  let _ = crate::state::with_state(|s| crate::image::deliver(s, reply, route));
}

/// Lands a planned presentation under a bypass policy (A-285): the presentation is recorded, the policy issues its
/// grant (`Issuer::Policy`), and the landing runs under that grant in the request that presented it, as a granted
/// landing's request does (`defer_granted_landing` takes the request and its reply route over). One transaction holds
/// the presentation, the grant and, when the landing could not start, the request's completion: a landing that starts
/// answers the request itself when it ends. The landing plans again under the grant, so a volume changed since the
/// presentation lands what the grant bound or is refused `GrantMismatch`, never something no one planned.
///
/// The reply and its route once the transaction commits, for the caller to deliver after the audit records the atom
/// owed the control partition (A-265); `None` when the landing took the request over, whose own completion answers it.
/// Then the atom's owed records are sent at once, so none waits on this shard's next atom.
fn land_under_policy(
  s: &mut ShardState,
  job: &PresentJob,
  request: (u64, slates_wire::request::RequestId),
  requested: Requested,
  plan: Box<slates_land::engine::Presented>,
) -> Option<(ReplyBody, Option<crate::merge_service::ReplyRoute>)> {
  let route = crate::image::take_in_flight(s, request);
  s.db.begin();
  s.current_request = Some(request);
  s.reply_route = route;
  let ids = LandingIds {
    landing_id: job.landing_id,
    fresh: true,
    volume: job.volume,
    snapshot: job.snapshot,
    target: &job.target,
    rebased: false,
  };
  let presenter = Presenter {
    client: job.client,
    principal: &job.principal,
    signature: job.signature.as_deref(),
  };
  let presented = present(s, presenter, &ids, plan);
  let reply = if matches!(presented, ReplyBody::GrantRequired { .. }) {
    // The grant needs to stand only until the landing that follows checks it: the node's failover delay (A-363,
    // A-414), the term the landing's own target lease runs under.
    let term = s.failover_ns;
    match issue_grant(
      s,
      &job.principal,
      job.landing_id,
      (GrantScope::Once, term),
      Issuer::Policy,
    ) {
      Ok(grant) => {
        s.count(POLICY_GRANTED_LAND, 1);
        land_verb_os(
          s,
          job.client,
          &job.principal,
          LandCall {
            volume: requested.volume,
            snapshot: requested.snapshot,
            target: &job.target,
            filter: &requested.filter,
            grant: Some(grant),
            present_only: false,
            signature: job.signature.as_deref(),
          },
        )
      }
      Err(refusal) => refused(refusal),
    }
  } else {
    presented
  };
  s.current_request = None;
  let route = s.reply_route.take();
  let (origin, id) = request;
  if std::mem::take(&mut s.acceptance_deferred) {
    // The granted landing took the request over and answers it when it ends; what led to it commits now, and the
    // presentation's and the grant's audit records go to the control partition now (A-265).
    if s.db.commit(&mut s.segment).is_err() {
      crate::verbs::reconcile_unpublished_effects(s);
    }
    send_owed_audit(s);
    return None;
  }
  let reply = crate::verbs::record_completion(s, origin, id, reply);
  let reply = match s.db.commit(&mut s.segment) {
    Ok(_) => reply,
    Err(e) => {
      crate::verbs::reconcile_unpublished_effects(s);
      refused(crate::error::refusal_of_db(&e))
    }
  };
  Some((reply, route))
}

/// The reply to a landing the engine did not land: its presentation when it needs a grant, else the typed
/// refusal.
fn not_landed(
  state: &mut ShardState,
  presenter: Presenter<'_>,
  ids: &LandingIds<'_>,
  refusal: LandingRefusal,
) -> ReplyBody {
  match refusal {
    LandingRefusal::GrantRequired(plan) => present(state, presenter, ids, plan),
    // The engine hands back every entry's report; the refusal names the conflicting ones alone (§4.15: a conflict
    // refuses the whole landing, and the person resolving it needs only the entries in conflict). Before 2026-10-06
    // every entry was named, so an entry that would only have been created read as a conflict.
    LandingRefusal::Conflict(entries) => {
      let conflicting: Vec<String> = entries
        .iter()
        .filter(|e| matches!(e.verdict, Some(slates_land::verdict::Verdict::Conflict(_))))
        .map(|e| e.path.to_string())
        .collect();
      // As many as one reply holds, the rest counted (A-267): presenting again lists every one.
      let room = crate::verbs::reply_room(&refused(Refusal::LandingConflict {
        entries: Vec::new(),
        entries_elided: 0,
      }));
      let (entries, entries_elided) = crate::verbs::carried(conflicting, room);
      refused(Refusal::LandingConflict {
        entries,
        entries_elided,
      })
    }
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
    LandingRefusal::Target(e) => refused(crate::error::target(crate::error::target_of_host(&e))),
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
      return refused(crate::error::target(crate::error::target_of_host(&e)));
    }
  };
  // The attempt is numbered in the landing-id shape — its partition in the high bits — so attempts of two
  // shards never share a holder.
  let holder = crate::verbs::landing_id(state.partition, state.landing.next_holder);
  state.landing.next_holder = state.landing.next_holder.saturating_add(1);
  let shard = state.shard;
  let control = state.shards.first().copied().unwrap_or(shard);
  // The term is the node's failover delay (A-363, A-414): a landing its daemon's end cancelled leaves its lease to the
  // term (AUD-29-03), so the target waits no longer than a failover would leave it unserved, while a running landing
  // renews it between its slices and during its barriers (`keep_lease_alive`, AUD-29-25).
  let term = state.failover_ns;
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
    // The landing's records reach the audit before its reply does (A-265); past the wait they stay owed, durable.
    if finished.is_some() {
      append_owed_audit(shard, term).await;
    }
    // The lease goes before the reply, so the caller's next landing into the target finds it free: waited in the control
    // shard's own time (A-432), so a control shard alive and busy is waited for and the lease never left to its term.
    let released = !held
      || crate::xshard::call_in_shard_time(
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
  /// When the landing last looked at its lease, on the shard's clock: at first when the lease was taken.
  lease_seen_ns: u64,
  /// The landing's signature, if its request carried one (A-321): judged against what it wrote when it finishes.
  signature: Option<Vec<u8>>,
  /// The audit ring's sequence when this run began (`Audit::next_seq`): the ring's records of the landing from it on are
  /// this run's, which its finish records; the ones before it are its presentation's, which the presentation recorded,
  /// or an earlier attempt's, which that attempt's finish recorded (§4.15 "Audit").
  audit_from: u64,
}

/// A granted landing that ended before running: what its reply and its trail need.
struct NotRun {
  refusal: Unrun,
  landing_id: u64,
  fresh: bool,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: String,
  principal: Principal,
  client: u32,
  /// The landing's signature, judged against a presentation its refusal answers with (A-321).
  signature: Option<Vec<u8>>,
  /// The grant it was to run under.
  grant: Option<u64>,
  /// The manifest its grant binds, when the shard held the grant as the attempt began: a granted landing, whose
  /// refusal its trail and its landing record keep ([`record_refused`]). `None` for a grant the shard does not hold
  /// (never issued, or ended): such a landing is not a granted one, and the engine presents it once planned.
  granted: Option<[u8; 32]>,
  /// The audit ring's sequence when it began ([`GrantedRun`]'s).
  audit_from: u64,
}

/// Why a granted landing ended before its run.
enum Unrun {
  /// The engine's refusal: of its beginning, its plan, its grant or its run's start.
  Engine(LandingRefusal),
  /// The server's, before the engine began: its target's lease, its volume gone, the head's snapshot, its base host
  /// away, its target unopened.
  Server(Refusal),
}

/// Where beginning a granted landing left it.
enum Begun {
  /// Being planned: stepped in slices until its run begins.
  Planning(Box<Planning>),
  /// Running: stepped in slices.
  Running(Box<GrantedRun>),
  /// Refused before running (its lease, the volume, the target, the grant): the reply and the trail are made where its
  /// completion is recorded.
  NotRun(Box<NotRun>),
}

/// What one slice left the run at.
enum Stepped {
  /// More host work remains.
  More,
  /// The run took a batch's barrier and waits on it (A-132): it runs on a thread of its own, never the shard.
  Barrier(Box<dyn Barrier>),
  /// Only the finish remains.
  Ready,
  /// The run ended early with this outcome (a refusal).
  Ended(Box<Result<LandingReport, LandingRefusal>>),
}

/// A granted landing's completed reply and where it goes. The audit records its completion owed the control partition
/// are in the partition's outbox, durable with the completion (A-265), and are sent before the reply.
type Finished = (ReplyBody, Option<crate::merge_service::ReplyRoute>);

/// Drives a granted landing under its lease: begins it, steps it one slice per turn of the shard (yielding
/// between, so the shard serves its other clients, lease checks and shutdown while a large landing runs),
/// and finishes it where its records and the request's completion commit as one atom.
async fn drive_granted_landing(
  prepared: Prepared,
  lease: Result<LandingLease, Refusal>,
  request: (u64, slates_wire::request::RequestId),
  cause: Option<slates_wire::observe::SpanContext>,
  (renew, key): (LeaseAsk, String),
) -> Option<Finished> {
  let mut granted = match start_granted(prepared, lease, renew, &key).await? {
    Begun::Running(granted) => granted,
    ended => return finish_unrun(request, cause, ended),
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
        keep_lease_alive(&mut *granted, renew, &key).await;
        slates_rt::futures::yield_now().await;
      }
      Stepped::Barrier(barrier) => {
        let ran = run_barrier_away(barrier, &mut granted, renew, &key).await;
        granted.run.barrier_ran(ran);
      }
      Stepped::Ready => {
        recall_before_finish(&mut granted, renew, &key).await;
        let outcome = advance_granted(&mut granted, renew, &key).await?;
        return crate::state::with_state(move |s| {
          complete_landing_with(s, request, cause, |s| complete_finish(s, *granted, outcome))
        });
      }
      Stepped::Ended(ended) => {
        return crate::state::with_state(move |s| {
          complete_landing_with(s, request, cause, |s| complete_finish(s, *granted, *ended))
        });
      }
    }
  }
}

/// A granted landing's finish: its O(entries) advance stepped a quantum at a time (A-133), the shard serving its other
/// clients between and the lease kept alive; the outcome its completion then commits as one atom. `None` when the
/// shard's state is gone (its shutdown).
async fn advance_granted(
  granted: &mut GrantedRun,
  renew: LeaseAsk,
  key: &str,
) -> Option<Result<LandingReport, LandingRefusal>> {
  loop {
    match crate::state::with_state(|s| {
      let stepped = step_finish(s, granted);
      crate::delegation::drain(s);
      stepped
    })? {
      FinishStepped::More => {
        keep_lease_alive(granted, renew, key).await;
        slates_rt::futures::yield_now().await;
      }
      FinishStepped::Done(outcome) => return Some(*outcome),
    }
  }
}

/// The reply to a granted landing that did not reach its run: its refusal, or what its not running is answered with.
fn answer_unrun(state: &mut ShardState, ended: Begun) -> ReplyBody {
  match ended {
    Begun::NotRun(not_run) => reply_not_run(state, *not_run),
    // `plan_granted` steps every plan to its end, so neither is left here: a defect, refused typed.
    Begun::Planning(_) | Begun::Running(_) => refused(Refusal::Unsupported {
      feature: "a granted landing answered before its plan or its run ended".to_owned(),
    }),
  }
}

/// Completes a granted landing that did not reach its run, its reply recorded with the request's completion.
fn finish_unrun(
  request: (u64, slates_wire::request::RequestId),
  cause: Option<slates_wire::observe::SpanContext>,
  ended: Begun,
) -> Option<Finished> {
  crate::state::with_state(move |s| {
    complete_landing_with(s, request, cause, |s| answer_unrun(s, ended))
  })
}

/// A granted landing begun ([`begin_granted`]) and its plan (§4.15 steps 1–3) stepped a slice at a time, its lease kept
/// alive between slices as its writes' is; what it began. `None` when the shard's state is gone (its shutdown).
async fn start_granted(
  prepared: Prepared,
  lease: Result<LandingLease, Refusal>,
  renew: LeaseAsk,
  key: &str,
) -> Option<Begun> {
  let begun = crate::state::with_state(move |s| begin_granted(s, prepared, lease))?;
  let Begun::Planning(mut planning) = begun else {
    return Some(begun);
  };
  loop {
    if let Some(begun) = crate::state::with_state(|s| step_planning(s, &mut planning))? {
      return Some(begun);
    }
    keep_lease_alive(&mut *planning, renew, key).await;
    slates_rt::futures::yield_now().await;
  }
}

/// The finish advances the volume's overlay past what reached the disk, which changes the landed files, so every
/// delegation held on a file of the volume is recalled first (RFC 8881 §10.2; A-79) and the landing waits for their
/// return, parked on the recall gate and keeping its target lease alive. A delegation not returned within a lease of
/// the recall is revoked (§10.4.5), by the drain at the deadline, and the finish runs. Before 2026-10-05 the finish
/// met the gate and the granted landing answered `Unpublished`, which `slates land` does not retry, and the recall it
/// asked for was never sent, since nothing drained after a landing task's slices.
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
      // The NFSv4 lease the listener advertises, which a delegation's holder is owed after its recall (A-425): the one
      // number every wait on a recall reads, not the landing's own lease term.
      Some((s.store.recall_gate.generation(), crate::nfs::v4_lease_ns(s)))
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
      let _ = crate::state::with_state_counted(crate::delegation::drain);
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
const LANDING_HELD_FOR_RECALL: &str = "landing.held_for_recall";

/// Format: how much of its term a landing's lease has left when the landing renews it, in permille — half:
/// the margin then covers a slice and the renewal's round trip to the control shard, each far shorter than
/// the term, while a landing renews at most twice a term.
const RENEW_AT_REMAINING_PERMILLE: u64 = 500;

/// The keepalive between a landing's slices (AUD-29-25; the lease records waited for it): once half its
/// lease's term has passed, the landing re-takes the lease on the control shard as its own holder and runs
/// under the renewed term. A lease another attempt took meanwhile (this one's ended) is not renewed; the run's
/// own per-entry fence then stops its writes, as before. Counted either way.
async fn keep_lease_alive(held: &mut impl KeptLease, ask: LeaseAsk, key: &str) {
  let expires_ns = held.lease().expires_ns;
  let margin = ask.term.saturating_mul(RENEW_AT_REMAINING_PERMILLE) / PERMILLE;
  let seen = *held.seen_ns();
  let Some(now) = crate::state::with_state(|s| {
    let now = s.clock.monotonic_ns();
    s.landing.longest_lease_gap_ns = s.landing.longest_lease_gap_ns.max(now.saturating_sub(seen));
    now
  }) else {
    return;
  };
  *held.seen_ns() = now;
  if now.saturating_add(margin) < expires_ns {
    return;
  }
  let target = key.to_owned();
  let deadline = now.saturating_add(ask.term);
  // Waited in the control shard's own time (A-432): its answer is the renewal or a typed refusal, a take run past its
  // deadline refusing itself (`take_target_lease`), so a busy control shard is waited for, never misread as gone.
  let renewed = crate::xshard::call_in_shard_time(
    ask.shard,
    ask.control,
    move |s| take_target_lease(s, target, ask.holder, ask.term, deadline),
    ask.term,
  )
  .await;
  let kept = match renewed {
    Some(Ok(lease)) => held.renew(lease),
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

/// A landing that holds its target's lease between its slices: a running one ([`GrantedRun`]), or one still being
/// planned ([`Planning`]); what [`keep_lease_alive`] keeps.
trait KeptLease {
  /// The lease it holds now.
  fn lease(&self) -> &LandingLease;
  /// Takes `renewed` as its lease, when it is this one's own (its target and holder); whether it did.
  fn renew(&mut self, renewed: LandingLease) -> bool;
  /// When it last looked at its lease, on the shard's clock.
  fn seen_ns(&mut self) -> &mut u64;
}

impl KeptLease for GrantedRun {
  fn lease(&self) -> &LandingLease {
    self.run.lease()
  }

  fn renew(&mut self, renewed: LandingLease) -> bool {
    self.run.renew_lease(renewed)
  }

  fn seen_ns(&mut self) -> &mut u64 {
    &mut self.lease_seen_ns
  }
}

impl KeptLease for Planning {
  fn lease(&self) -> &LandingLease {
    &self.lease
  }

  fn renew(&mut self, renewed: LandingLease) -> bool {
    if renewed.target != self.lease.target || renewed.holder != self.lease.holder {
      return false;
    }
    self.lease = renewed;
    true
  }

  fn seen_ns(&mut self) -> &mut u64 {
    &mut self.lease_seen_ns
  }
}

/// Format: the landing-plane counter of a lease a running landing renewed between its slices.
const LEASE_RENEWED: &str = "landing.lease_renewed";

/// Format: the landing-plane counter of a lease a running landing could not renew (another attempt holds the
/// target, or the control shard did not answer): its term then ends the landing's writes.
const LEASE_NOT_RENEWED: &str = "landing.lease_not_renewed";

/// Counter: a landing batch's barrier run on a thread of its own while the shard served on (A-132).
/// Format: a counter name in the daemon's status report.
const BARRIERS_AWAY: &str = "landing.barriers_away";

/// Counter: a barrier thread the system refused to start (a process or cgroup thread limit): the batch's entries
/// fail with the refusal and the landing stops writing, for its resume, as at any refused host step.
/// Format: a counter name in the daemon's status report.
const BARRIER_THREAD_REFUSED: &str = "landing.barrier_thread_refused";

/// A landing batch's barrier on a thread of its own (A-132), owned by the landing's task: the thread sends the
/// barrier's outcome over a channel of one and then wakes the task, so the wake always finds the outcome. Dropped
/// before the outcome came (a shutdown while the barrier waits), it joins the thread first, so no thread outlives
/// its owner; the join waits out the one barrier.
struct BarrierThread {
  outcome: std::sync::mpsc::Receiver<Result<(), HostError>>,
  /// Joined on the landing's shard, which is marked as waiting on it meanwhile (A-228's transitive wait).
  thread: Option<slates_rt::registry::WaitedThread<()>>,
}

impl BarrierThread {
  /// Starts `barrier` on its own thread, `waker` woken once its outcome is sent; refused with the system's errno when
  /// no thread can start (the barrier is dropped unrun, its descriptors closed).
  fn start(barrier: Box<dyn Barrier>, waker: std::task::Waker) -> Result<Self, HostError> {
    let (send, outcome) = std::sync::mpsc::sync_channel(1);
    let thread = slates_rt::registry::spawn_waited(
      std::thread::Builder::new().name("slates-land-barrier".to_owned()),
      move || {
        // The receiver outlives the thread (its owner joins before it drops), so the send cannot fail; were it to,
        // nobody is left to tell.
        if send.send(barrier.run()).is_ok() {
          waker.wake();
        }
      },
    )
    .map_err(|error| HostError::Unavailable(slates_vfs::host::OsCode::of_io(&error)))?;
    Ok(Self {
      outcome,
      thread: Some(thread),
    })
  }

  /// The barrier's outcome once it is back (its thread joined: it has only to exit), `None` while it runs. A thread
  /// gone without an outcome (it cannot be: shipped code does not panic, and a panic aborts) reads as the platform's
  /// failed I/O ([`lost_barrier`]).
  fn returned(&mut self) -> Option<Result<(), HostError>> {
    let ran = match self.outcome.try_recv() {
      Ok(ran) => ran,
      Err(std::sync::mpsc::TryRecvError::Empty) => return None,
      Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(lost_barrier()),
    };
    self.join();
    Some(ran)
  }

  fn join(&mut self) {
    if let Some(thread) = self.thread.take()
      && thread.join().is_err()
    {
      slates_machine::log_line!("slates-server: a landing barrier thread panicked");
    }
  }
}

/// What a barrier thread gone without its outcome reads as (it cannot be: shipped code does not panic, and a panic
/// aborts): the platform's own number for a failed I/O, `EIO` on Unix.
#[cfg(unix)]
fn lost_barrier() -> HostError {
  HostError::Unavailable(slates_vfs::host::OsCode::Errno(
    rustix::io::Errno::IO.raw_os_error(),
  ))
}

/// What a barrier thread gone without its outcome reads as: Windows' own number for a failed I/O,
/// `ERROR_IO_DEVICE`, since the Windows seam reports a refusal the engine does not tell apart in Win32's codes
/// (`slates_land::os`, A-185), typed as one (A-527).
#[cfg(windows)]
fn lost_barrier() -> HostError {
  /// Format: `ERROR_IO_DEVICE` (winerror.h).
  const ERROR_IO_DEVICE: u32 = 1117;
  HostError::Unavailable(slates_vfs::host::OsCode::Win32(ERROR_IO_DEVICE))
}

impl Drop for BarrierThread {
  fn drop(&mut self) {
    self.join();
  }
}

/// Runs a landing batch's barrier away from the shard and waits for it without holding the shard (A-132): the shard
/// serves its other clients meanwhile, and parks when it has none, where the barrier on the shard held every one of
/// them for its whole wait (1–160 ms a batch on a busy Linux machine, an `fsync` up to 206 ms a file on a busy Mac,
/// 2026-10-10). The landing's lease is kept alive while the barrier lasts, as between slices. The outcome: the
/// barrier's, or the refusal that no thread could start for it.
async fn run_barrier_away(
  barrier: Box<dyn Barrier>,
  granted: &mut GrantedRun,
  renew: LeaseAsk,
  key: &str,
) -> Result<(), HostError> {
  let waker = std::future::poll_fn(|cx| std::task::Poll::Ready(cx.waker().clone())).await;
  let started = BarrierThread::start(barrier, waker);
  crate::state::with_state(|s| {
    let counter = if started.is_ok() {
      BARRIERS_AWAY
    } else {
      BARRIER_THREAD_REFUSED
    };
    s.count(counter, 1);
  });
  let mut away = started?;
  loop {
    if let Some(ran) = away.returned() {
      return ran;
    }
    keep_lease_alive(granted, renew, key).await;
    // Woken by the thread, or at the lease's next renewal: whichever is first. With no renewal due ahead (refused, or
    // overdue), the thread's wake alone, never a spin.
    match renewal_wait_ns(granted, renew) {
      Some(wait) => {
        if slates_rt::futures::within(wait, slates_rt::futures::idle())
          .await
          .is_err()
        {
          slates_rt::futures::idle().await;
        }
      }
      None => slates_rt::futures::idle().await,
    }
  }
}

/// How long until the landing's lease is due its renewal (`keep_lease_alive`, Unix's), `None` when it is due already (a
/// renewal just refused leaves it so) or the shard's clock cannot be read.
fn renewal_wait_ns(granted: &GrantedRun, ask: LeaseAsk) -> Option<u64> {
  let margin = ask.term.saturating_mul(RENEW_AT_REMAINING_PERMILLE) / PERMILLE;
  let due = granted.run.lease().expires_ns.saturating_sub(margin);
  let now = crate::state::with_state(|s| s.clock.monotonic_ns())?;
  due.checked_sub(now).filter(|wait| *wait > 0)
}

/// Begins a granted landing on its owner shard: its id and request, the head's implicit snapshot for an
/// unnamed landing, the writer (inside an overlay's own base host), and the engine's plan, grant and lease
/// checks. The attempt's id and where its records start in the engine's ring are decided first, so every refusal
/// from here on, its target's lease included, ends as an attempt its trail names (§4.15 "Audit").
fn begin_granted(
  state: &mut ShardState,
  prepared: Prepared,
  lease: Result<LandingLease, Refusal>,
) -> Begun {
  // Where this run's records will start in the engine's audit ring, before it pushes any (§4.15 "Audit").
  let audit_from = state.landing.audit.next_seq();
  let presented = prepared.grant.and_then(|g| presentation_for(state, g));
  let landing_id = presented
    .unwrap_or_else(|| crate::verbs::landing_id(state.partition, state.landing.next_landing));
  let granted = prepared
    .grant
    .and_then(|grant| state.landing.grants.get(GrantId(grant)))
    .map(|grant| grant.manifest);
  let not_run = |refusal: Unrun, principal: Principal| {
    Begun::NotRun(Box::new(NotRun {
      refusal,
      landing_id,
      fresh: presented.is_none(),
      volume: prepared.volume,
      snapshot: prepared.snapshot,
      target: prepared.target.clone(),
      principal,
      client: prepared.client,
      signature: prepared.signature.clone(),
      grant: prepared.grant,
      granted,
      audit_from,
    }))
  };
  let lease = match lease {
    Ok(lease) => lease,
    Err(refusal) => return not_run(Unrun::Server(refusal), prepared.principal.clone()),
  };
  let against = match capture_of(state, prepared.volume) {
    Ok(against) => against,
    Err(refusal) => return not_run(Unrun::Server(refusal), prepared.principal.clone()),
  };
  let Ok(slot) = state.volumes.get_mut(prepared.handle) else {
    return not_run(Unrun::Server(Refusal::NotFound), prepared.principal.clone());
  };
  // An unnamed landing lands the head as it is now: a snapshot of it, which the writers that go on
  // between slices do not move.
  let (source, implicit) = match prepared.source {
    Source::Head => match slot.volume.snapshot(&mut state.store) {
      Ok(id) => (Source::Snapshot(id), Some(id)),
      Err(e) => {
        state.count(LANDING_SNAPSHOT_REFUSED, 1);
        return not_run(
          Unrun::Server(refusal_of_vfs(&e)),
          prepared.principal.clone(),
        );
      }
    },
    named => (named, None),
  };
  let was_overlay = slot.volume.is_overlay();
  let (os, land_target) = if was_overlay {
    let Some(host) = slot.host.take() else {
      drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
      return not_run(
        Unrun::Server(Refusal::BaseUnavailable {
          path: prepared.target.clone(),
          errno: 0,
        }),
        prepared.principal.clone(),
      );
    };
    match OsLand::open_target_in(host, std::path::Path::new(&prepared.target)) {
      Ok(pair) => pair,
      Err((host, refusal)) => {
        slot.host = Some(host);
        drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
        return not_run(
          Unrun::Server(target_refusal(&refusal, &prepared.target)),
          prepared.principal.clone(),
        );
      }
    }
  } else {
    (prepared.os, prepared.land_target)
  };
  let request = LandingRequest {
    landing_id,
    consumer: prepared.principal.key().into_boxed_slice(),
    volume: prepared.volume.bytes,
    snapshot: prepared.snapshot.value,
    source,
    grant: prepared.grant.map(GrantId),
    filter: prepared.filter.clone(),
    // Read as the beginning starts: its clock counts from `Beginning::new`, the plan included.
    now_ns: state.clock.monotonic_ns(),
    media_durability: false,
    large_class_bytes: state.config.large_class_bytes,
  };
  let beginning = match landing_beginning(&slot.volume, &land_target, &request, against) {
    Ok(beginning) => beginning,
    Err(refusal) => {
      give_host_back(slot, HostSlot::Owned(os), was_overlay, land_target.dir);
      drop_implicit(slot, &mut state.store, implicit, &mut state.refusals);
      return not_run(Unrun::Engine(refusal), prepared.principal.clone());
    }
  };
  let writer = if was_overlay {
    let (host, away) = os.lend();
    slot.host = Some(host);
    Writer::Lent(away)
  } else {
    Writer::Own(os)
  };
  Begun::Planning(Box::new(Planning {
    beginning,
    writer,
    lease_seen_ns: lease.expires_ns.saturating_sub(state.failover_ns),
    lease,
    handle: prepared.handle,
    landing_id,
    fresh: presented.is_none(),
    volume: prepared.volume,
    snapshot: prepared.snapshot,
    target: prepared.target,
    principal: prepared.principal,
    client: prepared.client,
    grant: prepared.grant,
    was_overlay,
    target_dir: land_target.dir,
    implicit,
    signature: prepared.signature,
    granted,
    audit_from,
  }))
}

/// A granted landing being planned between its slices ([`begin_granted`]): its beginning, its writer, and its lease,
/// kept alive between slices as a running landing's is.
struct Planning {
  beginning: slates_land::engine::Beginning,
  writer: Writer,
  lease: LandingLease,
  lease_seen_ns: u64,
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
  implicit: Option<slates_vfs::ids::SnapshotId>,
  /// The landing's signature, if its request carried one (A-321).
  signature: Option<Vec<u8>>,
  /// The manifest its grant binds, when the shard held the grant as it began ([`NotRun`]'s).
  granted: Option<[u8; 32]>,
  /// The audit ring's sequence when this run began ([`GrantedRun`]'s).
  audit_from: u64,
}

/// One slice of a granted landing's plan (`image::sliced`); what it began once planned: its run, or the refusal it
/// ended with (the volume's, the target's, the grant's, its lease's).
fn step_planning(state: &mut ShardState, planning: &mut Planning) -> Option<Begun> {
  let ended = match crate::image::sliced(state, |s| {
    Ok(step_beginning(
      s,
      planning.handle,
      &mut planning.writer,
      &mut planning.beginning,
    ))
  }) {
    Ok(Some(ended)) => ended,
    Ok(None) => return None,
    Err(refusal) => Err(LandingRefusal::Volume(match refusal {
      Refusal::NotFound => slates_vfs::error::VfsError::StaleHandle,
      _ => slates_vfs::error::VfsError::Invalid,
    })),
  };
  Some(finish_planning(state, planning, ended))
}

/// A granted landing's plan, ended: its run begun under its lease (`begin_planned`), the overlay's host lent back to
/// its slot between the run's slices; or, refused, its host given back, its implicit snapshot dropped, and the
/// refusal it is answered with.
fn finish_planning(
  state: &mut ShardState,
  planning: &mut Planning,
  ended: Result<slates_land::engine::Planned, LandingRefusal>,
) -> Begun {
  let not_run = |planning: &Planning, refusal: LandingRefusal| {
    Begun::NotRun(Box::new(NotRun {
      refusal: Unrun::Engine(refusal),
      landing_id: planning.landing_id,
      fresh: planning.fresh,
      volume: planning.volume,
      snapshot: planning.snapshot,
      target: planning.target.clone(),
      principal: planning.principal.clone(),
      client: planning.client,
      signature: planning.signature.clone(),
      grant: planning.grant,
      granted: planning.granted,
      audit_from: planning.audit_from,
    }))
  };
  let Ok(slot) = state.volumes.get_mut(planning.handle) else {
    return not_run(
      planning,
      LandingRefusal::Volume(slates_vfs::error::VfsError::StaleHandle),
    );
  };
  let os = match std::mem::replace(&mut planning.writer, Writer::Out) {
    Writer::Own(os) => Some(os),
    Writer::Lent(away) => slot.host.take().map(|host| OsLand::resume(host, away)),
    Writer::Out => None,
  };
  let Some(os) = os else {
    drop_implicit(
      slot,
      &mut state.store,
      planning.implicit,
      &mut state.refusals,
    );
    return not_run(planning, LandingRefusal::HostAway);
  };
  let plan = match ended {
    Ok(slates_land::engine::Planned::Granted(plan)) => plan,
    Ok(slates_land::engine::Planned::Presented(presented)) => {
      give_host_back(
        slot,
        HostSlot::Owned(os),
        planning.was_overlay,
        planning.target_dir,
      );
      drop_implicit(
        slot,
        &mut state.store,
        planning.implicit,
        &mut state.refusals,
      );
      return not_run(planning, LandingRefusal::GrantRequired(presented));
    }
    Err(refusal) => {
      give_host_back(
        slot,
        HostSlot::Owned(os),
        planning.was_overlay,
        planning.target_dir,
      );
      drop_implicit(
        slot,
        &mut state.store,
        planning.implicit,
        &mut state.refusals,
      );
      return not_run(planning, refusal);
    }
  };
  let telemetry_capacity = usize::try_from(state.config.region.slots)
    .unwrap_or(1)
    .max(1);
  match slates_land::engine::begin_planned(HostSlot::Owned(os), Some(&planning.lease), *plan) {
    Ok(run) => {
      let mut granted = GrantedRun {
        run,
        handle: planning.handle,
        landing_id: planning.landing_id,
        fresh: planning.fresh,
        volume: planning.volume,
        snapshot: planning.snapshot,
        target: planning.target.clone(),
        principal: planning.principal.clone(),
        client: planning.client,
        grant: planning.grant,
        was_overlay: planning.was_overlay,
        target_dir: planning.target_dir,
        spans: SpanObserver::with_capacity(telemetry_capacity),
        away: None,
        implicit: planning.implicit,
        lease_seen_ns: planning.lease_seen_ns,
        signature: planning.signature.clone(),
        audit_from: planning.audit_from,
      };
      if planning.was_overlay {
        lend_back(slot, &mut granted);
      }
      Begun::Running(Box::new(granted))
    }
    Err((host, refusal)) => {
      give_host_back(slot, host, planning.was_overlay, planning.target_dir);
      drop_implicit(
        slot,
        &mut state.store,
        planning.implicit,
        &mut state.refusals,
      );
      not_run(planning, refusal)
    }
  }
}

/// One unit of a landing's beginning (`slates_land::engine::Beginning`) on the volume at `handle`, its writer taken for
/// the unit and given back after (an overlay's host back to the volume's slot, so the volume serves between units);
/// its end once it has one: the plan, or the landing refusal that ended it (a volume destroyed meanwhile, an overlay
/// whose host is away, the volume's or the target's own). A presentation's and a granted landing's plan both step
/// here.
fn step_beginning(
  state: &mut ShardState,
  handle: slates_mem::Handle<crate::state::VolumeSlot>,
  writer: &mut Writer,
  beginning: &mut slates_land::engine::Beginning,
) -> Option<Result<slates_land::engine::Planned, LandingRefusal>> {
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return Some(Err(LandingRefusal::Volume(
      slates_vfs::error::VfsError::StaleHandle,
    )));
  };
  let mut os = match std::mem::replace(writer, Writer::Out) {
    Writer::Own(os) => os,
    Writer::Lent(away) => match slot.host.take() {
      Some(host) => OsLand::resume(host, away),
      None => {
        *writer = Writer::Lent(away);
        return Some(Err(LandingRefusal::HostAway));
      }
    },
    Writer::Out => return Some(Err(LandingRefusal::HostAway)),
  };
  let stepped = beginning.unit(
    &mut os,
    &mut slot.volume,
    &mut state.store,
    &mut state.landing.grants,
    &mut state.landing.audit,
  );
  *writer = if slot.volume.is_overlay() {
    let (host, away) = os.lend();
    slot.host = Some(host);
    Writer::Lent(away)
  } else {
    Writer::Own(os)
  };
  stepped.transpose()
}

/// Gives an overlay's base host back to its volume's slot between slices, keeping the writer's own state.
fn lend_back(slot: &mut crate::state::VolumeSlot, granted: &mut GrantedRun) {
  if let Some(os) = granted.run.take_host() {
    let (host, away) = os.lend();
    slot.host = Some(host);
    granted.away = Some(away);
  }
}

/// Takes an overlay's base host from its volume's slot back into the run for a slice; whether it could.
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
const IMPLICIT_SNAPSHOT_KEPT: &str = "landing.implicit_snapshot_kept";
/// Format: the refusal-ledger names of a landing refused at each of its volume stages — the implicit snapshot
/// of the head, the engine's read of the volume, the publication after it — so a refusal names its stage
/// (a `NoSpace` seen only on the macOS runner, 2026-10-01, carried no stage).
const LANDING_SNAPSHOT_REFUSED: &str = "landing.snapshot_refused";
/// Format: see [`LANDING_SNAPSHOT_REFUSED`].
const LANDING_VOLUME_REFUSED: &str = "landing.volume_refused";
/// Format: see [`LANDING_SNAPSHOT_REFUSED`].
const LANDING_PUBLISH_REFUSED: &str = "landing.publish_refused";

/// Format: the share of the shard's step quantum one landing slice takes, in permille — half, as the
/// archive walk's slice takes (`config::archive_slice_bytes`), so a slice leaves the rest of the shard's
/// step to its other work.
const LANDING_SLICE_PERMILLE: u64 = 500;
/// Format: the permille scale.
const PERMILLE: u64 = 1_000;

/// One slice of a granted landing: the run stepped for its share of the shard's quantum, the base host lent
/// back after. A volume gone meanwhile (destroyed) or whose base host is away ends the run, its entries left in
/// the overlay as a crash leaves them.
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
    None => match granted.run.take_barrier() {
      Some(barrier) => Stepped::Barrier(barrier),
      None if granted.run.ready_to_finish() => Stepped::Ready,
      None => Stepped::More,
    },
  }
}

/// Finishes a granted landing inside its completion's atom: the run's finish (unless it `ended` early), its
/// once-grant consumed, its spans drained, the writer's host settled, the implicit snapshot destroyed, and
/// the reply with the landing's records.
/// Where one step of the sliced finish left the advance (A-133).
enum FinishStepped {
  /// The advance has more entries; the caller yields and steps again.
  More,
  /// The finish is done (or failed), with this outcome to complete.
  Done(Box<Result<LandingReport, LandingRefusal>>),
}

/// One step of a granted landing's finish (A-133): the overlay advanced past what reached the disk, for a slice of the
/// shard's quantum, the base host lent back after — the same shape as [`step_granted`], so the shard serves its other
/// clients between the advance's steps rather than holding them for its whole O(entries) run. A volume gone or a host
/// away ends the finish with that refusal.
fn step_finish(state: &mut ShardState, granted: &mut GrantedRun) -> FinishStepped {
  let budget_ns = state
    .config
    .step_quantum_ns()
    .saturating_mul(LANDING_SLICE_PERMILLE)
    / PERMILLE;
  let Ok(slot) = state.volumes.get_mut(granted.handle) else {
    granted.run.abandon();
    return FinishStepped::Done(Box::new(Err(LandingRefusal::Volume(
      slates_vfs::error::VfsError::StaleHandle,
    ))));
  };
  if granted.was_overlay && !take_for_slice(slot, granted) {
    granted.run.abandon();
    return FinishStepped::Done(Box::new(Err(LandingRefusal::HostAway)));
  }
  let began = state.clock.monotonic_ns();
  let finished = granted.run.finish_step(
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
  if granted.run.last_step_ns() > budget_ns.saturating_add(granted.run.last_unit_ns()) {
    state.landing.slices_past_budget = state.landing.slices_past_budget.saturating_add(1);
  }
  match finished {
    Some(outcome) => FinishStepped::Done(Box::new(outcome)),
    None => FinishStepped::More,
  }
}

/// Completes a granted landing once its advance is done (A-133): its grant settled, its spans drained, its writer's host
/// settled back into the volume's slot, the implicit snapshot destroyed, and the reply with the landing's records (the
/// completion commits as one atom around this, A-127). `outcome` is the finish's, or the refusal a run that ended early
/// carried.
fn complete_finish(
  state: &mut ShardState,
  mut granted: GrantedRun,
  outcome: Result<LandingReport, LandingRefusal>,
) -> ReplyBody {
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
    signature: granted.signature.as_deref(),
  };
  match outcome {
    Ok(report) => finish(
      state,
      FinishedRun {
        principal: &granted.principal,
        signature: granted.signature.as_deref(),
        audit_from: granted.audit_from,
      },
      &ids,
      &report,
      granted.grant,
    ),
    Err(refusal) => {
      if ends_refused(&refusal) {
        let attempt = Attempt {
          landing_id: granted.landing_id,
          fresh: granted.fresh,
          grant: granted.grant,
          granted: granted.run.grant().manifest,
          principal: &granted.principal,
          volume: granted.volume,
          snapshot: granted.snapshot,
          target: &granted.target,
          signature: granted.signature.as_deref(),
          audit_from: granted.audit_from,
          conflicts: conflicts_of(&refusal),
        };
        if let Err(e) = record_refused(state, &attempt) {
          return refused(crate::error::refusal_of_db(&e));
        }
      }
      not_landed(state, presenter, &ids, refusal)
    }
  }
}

/// The reply to a granted landing refused before it ran, its trail recorded first ([`record_refused`]).
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
    signature: not_run.signature.as_deref(),
  };
  let (ended, conflicts) = match &not_run.refusal {
    Unrun::Engine(refusal) => (ends_refused(refusal), conflicts_of(refusal)),
    Unrun::Server(_) => (true, 0),
  };
  // Only a granted landing leaves a trail: one whose grant the shard held as it began. A landing under a grant the shard
  // does not hold is presented once planned (the engine's `GrantRequired`), so refused before its plan it is a refused
  // request, as an ungranted one is.
  if let Some(granted) = not_run.granted
    && ended
  {
    let attempt = Attempt {
      landing_id: not_run.landing_id,
      fresh: not_run.fresh,
      grant: not_run.grant,
      granted,
      principal: &not_run.principal,
      volume: not_run.volume,
      snapshot: not_run.snapshot,
      target: &not_run.target,
      signature: not_run.signature.as_deref(),
      audit_from: not_run.audit_from,
      conflicts,
    };
    if let Err(e) = record_refused(state, &attempt) {
      return refused(crate::error::refusal_of_db(&e));
    }
  }
  match not_run.refusal {
    Unrun::Engine(refusal) => not_landed(state, presenter, &ids, refusal),
    Unrun::Server(refusal) => refused(refusal),
  }
}

/// Whether the engine's `refusal` ends a granted landing's attempt refused, which its trail records ([`record_refused`]).
/// A plan no grant covers ends in a presentation instead, which records itself.
fn ends_refused(refusal: &LandingRefusal) -> bool {
  !matches!(refusal, LandingRefusal::GrantRequired(_))
}

/// Who a granted landing's attempt was, as its trail and its landing record name it: its id, its grant and the
/// manifest the grant binds, whom it ran for, what it would have landed where, its signature, where its records start
/// in the engine's ring, and the entries its refusal found in conflict.
struct Attempt<'a> {
  landing_id: u64,
  /// Whether the id is a fresh one rather than its presentation's.
  fresh: bool,
  grant: Option<u64>,
  /// The manifest its grant binds: what it was approved to land, named when it planned nothing.
  granted: [u8; 32],
  principal: &'a Principal,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: &'a str,
  signature: Option<&'a [u8]>,
  /// The audit ring's sequence when the attempt began ([`GrantedRun`]'s `audit_from`).
  audit_from: u64,
  /// The entries a conflict refused it for; zero for any other refusal.
  conflicts: u32,
}

/// The entries `refusal` names in conflict (§4.15 step 4): a conflict's entries whose verdict is a conflict, as its
/// answer names them; zero for any other refusal.
fn conflicts_of(refusal: &LandingRefusal) -> u32 {
  match refusal {
    LandingRefusal::Conflict(entries) => u32::try_from(
      entries
        .iter()
        .filter(|entry| {
          matches!(
            entry.verdict,
            Some(slates_land::verdict::Verdict::Conflict(_))
          )
        })
        .count(),
    )
    .unwrap_or(u32::MAX),
    _ => 0,
  }
}

/// Records a refused granted landing's end (§4.15 "Audit", the state machine's `→ Refused` and `→ Aborted`), in its
/// completion's atom. Two things are written:
/// - **The trail.** The records the engine's ring holds of the attempt from its beginning on (its plan, its validation,
///   each entry it wrote or refused), then a `LandingFinished`. That record names the terminal state
///   ([`refused_state`]), the manifest the attempt planned, or its grant's when it planned none, and its signature
///   judged against that manifest. An attempt whose ring records hold its end already adds none.
/// - **The landing record, at the same terminal state.** Its presentation's record moves there, and a fresh id's is
///   made, so the record and the audit cannot disagree. A fresh id is spent, as a finished landing's is, and the
///   record keeps it spent across a restart (`verbs::next_landing_counter`).
///
/// Before 2026-11-18 a refused granted landing recorded none of this: its trail ended at its grant (found by g90
/// reading the code; `docs/bugs/2026-11-18-a-refused-granted-landing-left-no-trail.md`). Until a later change the same
/// day its record stayed as presented, or was never made, while its trail said it was refused.
fn record_refused(state: &mut ShardState, attempt: &Attempt<'_>) -> Result<(), slates_db::DbError> {
  let ring: Vec<AuditRecord> = state
    .landing
    .audit
    .records()
    .into_iter()
    .filter(|record| record.landing == attempt.landing_id && record.seq >= attempt.audit_from)
    .cloned()
    .collect();
  let manifest = ring
    .iter()
    .rev()
    .find(|record| record.kind == AuditKind::LandingPlanned)
    .map_or(attempt.granted, |record| record.manifest);
  let (_, verification) = landing_verdict(
    state,
    (attempt.volume, attempt.target, &manifest),
    attempt.signature,
  );
  let ended = refused_state(&ring);
  let written = u32::try_from(
    ring
      .iter()
      .filter(|record| record.kind == AuditKind::EntryWritten)
      .count(),
  )
  .unwrap_or(u32::MAX);
  let mut records: Vec<DbAuditRecord> = ring
    .iter()
    .map(|record| db_audit(record, attempt.principal, &verification))
    .collect();
  let now = state.clock.monotonic_ns();
  if !ring
    .iter()
    .any(|record| record.kind == AuditKind::LandingFinished)
  {
    records.push(DbAuditRecord {
      seq: 0,
      at_ns: now,
      kind: DbAuditKind::LandingFinished,
      principal: attempt.principal.clone(),
      grant: attempt.grant,
      landing: Some(attempt.landing_id),
      manifest: Some(manifest),
      outcome: Some(ended),
      signature: crate::signed_work::to_record(&verification),
    });
  }
  let landing = if state.db.partition().landing(attempt.landing_id).is_some() {
    Op::LandingStateChanged {
      id: attempt.landing_id,
      state: ended,
      written,
      conflicts: attempt.conflicts,
    }
  } else {
    Op::LandingRecorded {
      record: LandingRecord {
        id: attempt.landing_id,
        volume: attempt.volume,
        snapshot: attempt.snapshot,
        target: attempt.target.to_owned(),
        manifest,
        grant: attempt.grant,
        state: ended,
        written,
        conflicts: attempt.conflicts,
      },
    }
  };
  state.db.mutate(&mut state.segment, &landing, now)?;
  if attempt.fresh {
    state.landing.next_landing = state.landing.next_landing.saturating_add(1);
  }
  write_audit(state, records, now)
}

/// The terminal state a refused granted landing's trail names (§4.15's state machine): `Aborted` once the attempt wrote
/// an entry, whose bytes the disk now holds, each entry old or new, for a resume to continue; else `Refused`, nothing
/// written.
fn refused_state(ring: &[AuditRecord]) -> DbLandingState {
  if ring
    .iter()
    .any(|record| record.kind == AuditKind::EntryWritten)
  {
    DbLandingState::Aborted
  } else {
    DbLandingState::Refused
  }
}

/// Where a landing attempt asks for its target's lease: from this shard to the control shard, for the
/// attempt `holder`, for `term` nanoseconds, answered by the monotonic `deadline`.
#[derive(Clone, Copy)]
struct LeaseAsk {
  shard: u16,
  control: u16,
  holder: u64,
  term: u64,
  deadline: u64,
}

/// Takes the target lease `key` from the control shard for a landing attempt, waiting in the control shard's own time
/// (A-432): a term, then on while the shard works or waits for a CPU, up to a term of its CPU, so a busy control shard
/// answers the take or refuses it typed (a take run past its deadline refuses itself), where a wall term's end left the
/// outcome unknown. A take whose answer still does not come (the shard stopped, or ran its budget) is compensated — the
/// attempt's own lease released, should the take have landed — so a lease outlives an attempt that never learned of it
/// only when the compensation fails as well, and then by its term.
async fn acquire_target_lease(ask: LeaseAsk, key: String) -> Result<LandingLease, Refusal> {
  let LeaseAsk {
    shard,
    control,
    holder,
    term,
    deadline,
  } = ask;
  let asked = key.clone();
  if let Some(taken) = crate::xshard::call_in_shard_time(
    shard,
    control,
    move |s| take_target_lease(s, asked, holder, term, deadline),
    term,
  )
  .await
  {
    return taken;
  }
  let compensated = crate::xshard::call_in_shard_time(
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
const LEASE_TAKE_UNANSWERED: &str = "landing.lease_take_unanswered";

/// Format: the landing-plane counter of a target lease whose release did not reach the control shard (it
/// ends by its term).
const LEASE_UNRELEASED: &str = "landing.lease_unreleased";

/// The placeholder a deferred landing's verb returns. It is never delivered: the verb deferred its reply, and
/// `run_recorded` answers nothing for it. A deferred attach's verb returns it too (the FUSE and WinFsp mounts).
pub(crate) fn deferred_reply() -> ReplyBody {
  ReplyBody::Landed {
    outcome: slates_ipc::protocol::LandingOutcome {
      landing: 0,
      state: String::new(),
      written: 0,
      skipped: 0,
      conflicts: 0,
      failed: 0,
      bytes_written: 0,
      held: 0,
      durability: slates_ipc::protocol::LandingDurability {
        data_synced: false,
        dirs_synced: false,
        media: false,
        media_requested: false,
        dirs: 0,
      },
      degraded: Vec::new(),
      barriers: 0,
      stage: slates_ipc::protocol::LandingStage::Planning,
      verification: slates_ipc::protocol::Verification::Unsigned,
      degraded_elided: 0,
    },
  }
}

/// A granted landing's completion on its owner shard: the reply `produce` builds — the finished landing's
/// records, or a refusal — and the request's completion committed as one atom. Returns the recorded reply
/// and where it goes.
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
fn deliver_landing(
  state: &mut ShardState,
  reply: ReplyBody,
  route: Option<crate::merge_service::ReplyRoute>,
) {
  let Some(route) = route else {
    return;
  };
  if crate::xshard::hand_reply_home(route.shard, move || {
    crate::state::deliver(route.client_index, route.request, reply, true);
  })
  .is_err()
  {
    // The client's retry answers from the completion record.
    state.count(LANDING_UNDELIVERED, 1);
  }
}

/// Format: the landing-plane counter of a granted landing's reply that could not be handed to its client's
/// shard.
const LANDING_UNDELIVERED: &str = "landing.reply_undelivered";

/// The refusal-ledger name of a landing refused for a grant bound elsewhere, by the field that differed.
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
struct Presenter<'a> {
  client: u32,
  principal: &'a Principal,
  /// The request's signature (A-321), judged against the presented manifest.
  signature: Option<&'a [u8]>,
}

/// Whether a presentation by `client` of `volume` into `target` would pass the shard's bound: a client's
/// re-presentation of the same volume and target replaces its own and never does (AUD-29-07).
fn presentations_full(state: &ShardState, client: u32, volume: DbVolumeId, target: &str) -> bool {
  let replaces = state.landing.awaiting.values().any(|awaiting| {
    awaiting.client == client && awaiting.volume == volume && awaiting.target == target
  });
  !replaces && state.landing.awaiting.len() >= state.config.landings_awaiting_per_shard
}

/// The presentation a granted landing lands (AUD-29-07): the awaiting landing whose manifest and binding the
/// grant was issued from. `None` for a grant covering a plan no presentation made (a session grant's later
/// plans), which lands under a fresh id.
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

fn present(
  state: &mut ShardState,
  presenter: Presenter<'_>,
  ids: &LandingIds<'_>,
  plan: Box<slates_land::engine::Presented>,
) -> ReplyBody {
  let slates_land::engine::Presented {
    manifest,
    preliminary,
    binding,
  } = *plan;
  // §4.15 step 2: the conflicts the engine's preliminary verdict pass found (step 4 run without writing), each with
  // its class, so the human deciding on the grant decides on evidence. Until 2026-10-08 the reply carried none, and a
  // plan over a disk an outsider had changed looked clean until the granted landing refused.
  let conflicts: Vec<String> = preliminary
    .iter()
    .filter_map(|entry| match entry.verdict {
      Some(slates_land::verdict::Verdict::Conflict(class)) => {
        Some(format!("{} ({class:?})", entry.path))
      }
      _ => None,
    })
    .collect();
  let Presenter {
    client,
    principal,
    signature,
  } = presenter;
  state.landing.next_landing = state.landing.next_landing.saturating_add(1);
  let hash = manifest.hash;
  // Signed work (A-321): the request's signature judged against the manifest presented, the verdict on the presentation's
  // audit record, the statement handed back for a signer.
  let (statement, verification) =
    landing_verdict(state, (ids.volume, ids.target, &hash), signature);
  let summary = summary_of(&manifest);
  let landing_id = ids.landing_id;
  // The presentation's reply must fit one reply whatever it carries (A-267), and its statement names the target
  // (A-321): a target too long to be answered beside this summary and verdict is refused here, before anything is
  // recorded, with the longest this presentation's reply holds.
  if let Some(longest) = presentation_overflow(
    &ReplyBody::GrantRequired {
      landing: landing_id,
      manifest: hash,
      summary: summary.clone(),
      conflicts: Vec::new(),
      statement: statement.clone(),
      verification,
      conflicts_elided: u32::MAX,
    },
    ids.target.len(),
  ) {
    return refused(Refusal::Target {
      refusal: slates_ipc::protocol::TargetRefusal::TooLong { longest },
    });
  }
  // A client presenting the same volume and target again replaces its earlier presentation, whose id is no
  // longer grantable (AUD-29-07): re-planning costs no room.
  state.landing.awaiting.retain(|_, awaiting| {
    !(awaiting.client == client && awaiting.volume == ids.volume && awaiting.target == ids.target)
  });
  state.landing.awaiting.insert(
    landing_id,
    Awaiting {
      manifest: hash,
      binding,
      volume: ids.volume,
      snapshot: ids.snapshot,
      target: ids.target.to_owned(),
      principal: principal.clone(),
      client,
      // The full entry list on demand (§4.15 step 2): the manifest's own entries, moved here.
      entries: manifest.entries,
      conflicts: crate::landing_entries::conflicts_of(&preliminary),
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
    conflicts: u32::try_from(conflicts.len()).unwrap_or(u32::MAX),
  };
  let planned = DbAuditRecord {
    seq: 0,
    at_ns: now,
    kind: DbAuditKind::LandingPlanned,
    principal: principal.clone(),
    grant: None,
    landing: Some(landing_id),
    manifest: Some(hash),
    outcome: None,
    signature: crate::signed_work::to_record(&verification),
  };
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::LandingRecorded { record }, now)
    .and_then(|_| write_audit(state, vec![planned], now))
  {
    return refused(crate::error::refusal_of_db(&e));
  }
  // The presentation is recorded, so its reply must always fit (A-267): the conflicts one reply holds beside the
  // summary, the rest counted; every entry, its conflict among it, is read a page at a time (`LandingEntries`).
  let room = crate::verbs::reply_room(&ReplyBody::GrantRequired {
    landing: landing_id,
    manifest: hash,
    summary: summary.clone(),
    conflicts: Vec::new(),
    statement: statement.clone(),
    verification,
    conflicts_elided: 0,
  });
  let (conflicts, conflicts_elided) = crate::verbs::carried(conflicts, room);
  ReplyBody::GrantRequired {
    landing: landing_id,
    manifest: hash,
    summary,
    conflicts,
    statement,
    verification,
    conflicts_elided,
  }
}

/// How a landing's signature is judged (signed work, A-321): over the landing's statement — the volume, the target as its
/// request named it and `manifest` — under the volume's trusted signers, which this shard, the volume's owner, holds.
/// Counted per verdict. The statement goes with the verdict, for a presentation's answer.
fn landing_verdict(
  state: &mut ShardState,
  (volume, target, manifest): (DbVolumeId, &str, &[u8; 32]),
  signature: Option<&[u8]>,
) -> (Vec<u8>, slates_ipc::protocol::Verification) {
  let statement = crate::signed_work::landing_statement(&volume.bytes, target, manifest);
  let verification = match signature {
    None => slates_ipc::protocol::Verification::Unsigned,
    Some(signature) => {
      let trusted = crate::signed_work::trusted_of(crate::verbs::signers_of(state, volume));
      crate::signed_work::judge(Some(signature), &statement, &trusted)
    }
  };
  if let Some(counter) = crate::signed_work::counter(&verification) {
    state.count(counter, 1);
  }
  (statement, verification)
}

/// What a granted run's finish records with it: whom it ran for, its signature, and where its records start in the
/// engine's audit ring.
struct FinishedRun<'a> {
  /// The principal the landing ran for.
  principal: &'a Principal,
  /// The landing's signature, if its request carried one (A-321).
  signature: Option<&'a [u8]>,
  /// The audit ring's sequence when the run began ([`GrantedRun`]'s `audit_from`).
  audit_from: u64,
}

/// Persists a finished landing: the record, its terminal audit, and the consumed grant.
fn finish(
  state: &mut ShardState,
  run: FinishedRun<'_>,
  ids: &LandingIds<'_>,
  report: &LandingReport,
  grant: Option<u64>,
) -> ReplyBody {
  let FinishedRun {
    principal,
    signature,
    audit_from,
  } = run;
  let landing_id = ids.landing_id;
  // Signed work (A-321): the landing's signature judged against the manifest it wrote, the verdict on its finished
  // record and in its answer.
  let (_, verification) = landing_verdict(
    state,
    (ids.volume, ids.target, &report.manifest_hash),
    signature,
  );
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
  ops.extend(closing_ops(state, ids, grant));
  // Mirror this run's audit records into the durable log, on the control partition (A-265), each once: the ring's
  // records of the landing from the run's beginning on. Before 2026-11-18 every record the ring held for the landing's
  // id was mirrored, so a presented landing's plan, which its presentation had recorded, was recorded again, and a
  // resumed landing re-recorded its first attempt.
  let records: Vec<DbAuditRecord> = state
    .landing
    .audit
    .records()
    .into_iter()
    .filter(|audit| audit.landing == landing_id && audit.seq >= audit_from)
    .map(|audit| db_audit(audit, principal, &verification))
    .collect();
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return refused(crate::error::refusal_of_db(&e));
    }
  }
  if let Err(e) = write_audit(state, records, now) {
    return refused(crate::error::refusal_of_db(&e));
  }
  // A grant whose consumption committed leaves the runtime table as it left the durable one (A-266).
  end_grants_recorded_ended(state, &ops);
  if let Some(refusal) = publish_landed(state, ids, report) {
    return refusal;
  }
  landed_reply(
    LandingOutcome {
      landing: landing_id,
      state: landing_state_name(db_state),
      stage: wire_landing_stage(db_state),
      written: report.written as u64,
      skipped: report.skipped as u64,
      conflicts: report.conflicts as u64,
      failed: report.failed as u64,
      bytes_written: report.bytes_written,
      held: u64::try_from(report.held).unwrap_or(u64::MAX),
      durability: durability_of(&report.durability),
      degraded: Vec::new(),
      barriers: u64::try_from(report.barriers).unwrap_or(u64::MAX),
      verification,
      degraded_elided: 0,
    },
    report.degraded.iter().map(degradation_of).collect(),
  )
}

/// A finished landing's reply (§4.15 step 9): `outcome` with as many of `degraded` as one reply holds beside it, the
/// rest counted (A-267). The landing has run, so its reply must always fit: a host that refuses every directory's sync
/// meets a degraded cell a directory, and a landing over hundreds of directories met more than a reply holds.
pub(crate) fn landed_reply(
  outcome: LandingOutcome,
  degraded: Vec<slates_ipc::protocol::LandingDegradation>,
) -> ReplyBody {
  let room = crate::verbs::reply_room(&ReplyBody::Landed {
    outcome: outcome.clone(),
  });
  let (degraded, degraded_elided) = crate::verbs::carried(degraded, room);
  ReplyBody::Landed {
    outcome: LandingOutcome {
      degraded,
      degraded_elided,
      ..outcome
    },
  }
}

/// Ends in the runtime table every grant `ops` recorded at a terminal step (A-266), once those ops committed.
fn end_grants_recorded_ended(state: &mut ShardState, ops: &[Op]) {
  for op in ops {
    if let Op::GrantStateChanged { id, state: ended } = op
      && !matches!(ended, DbGrantState::Issued)
    {
      state.landing.grants.end(slates_land::grant::GrantId(*id));
    }
  }
}

/// The grants past their term end (A-266), by the shard's deadline keeper at the first instant past the earliest term
/// (A-554, `crate::deadlines`; the reap loop's tick until then): each is recorded `Expired`, which takes it out of the
/// durable table, with its `grant_expired` audit record in the same atom ([`record_expiry`]), and then leaves the
/// runtime one. A grant that cannot be recorded stays due, and the keeper tries again a retry window on; one in use by
/// a running landing is unaffected, since the run holds its own copy of the grant it checked. On a shard other than the
/// control shard each committed expiry's audit record is owed in its own atom (the partition's outbox), and what is
/// owed goes to the control partition as one batch once the sweep has ended any.
pub fn end_expired_grants(state: &mut ShardState) {
  let now = state.clock.monotonic_ns();
  let mut ended = false;
  for id in state.landing.grants.expired_at(now) {
    if record_expiry(state, id.0, now) {
      state.landing.grants.end(id);
      state.count(GRANTS_EXPIRED, 1);
      ended = true;
    }
  }
  if ended {
    send_owed_audit(state);
  }
}

/// Records grant `id`'s expiry and its audit record as one atom (A-266; AC-2.10, every grant's history in the audit):
/// whether it committed. The record names the principal the grant was made for, since no one acted, and the manifest it
/// bound. A grant the durable table does not hold is not recorded; a refused change applies nothing, so the atom
/// commits nothing; and a refused commit rolls both back, the owed audit record with them. The audit record cannot be
/// refused once the change applied: an `AuditAppended` is never refused and its apply cannot fail, and on another
/// shard it is only owed.
fn record_expiry(state: &mut ShardState, id: u64, now: u64) -> bool {
  let Some((principal, manifest)) = state
    .db
    .partition()
    .grant(id)
    .map(|grant| (grant.principal.clone(), grant.manifest))
  else {
    return false;
  };
  let audit = DbAuditRecord {
    seq: 0,
    at_ns: now,
    kind: DbAuditKind::GrantExpired,
    principal,
    grant: Some(id),
    landing: None,
    manifest: Some(manifest),
    outcome: None,
    // No landing judged here: a grant's end is no landing's record (A-321's signature is on those alone).
    signature: None,
  };
  let op = Op::GrantStateChanged {
    id,
    state: DbGrantState::Expired,
  };
  state.db.begin();
  let applied = state
    .db
    .mutate(&mut state.segment, &op, now)
    .and_then(|_| write_audit(state, vec![audit], now));
  match state.db.commit(&mut state.segment) {
    Ok(_) => applied.is_ok(),
    Err(e) => {
      let refusal = crate::error::refusal_of_db(&e);
      state.count(crate::verbs::refusal_name(&refusal), 1);
      crate::verbs::reconcile_unpublished_effects(state);
      false
    }
  }
}

/// Counter: grants ended at their term by the deadline keeper's sweep (A-266, A-554).
pub const GRANTS_EXPIRED: &str = "landing.grants_expired";

/// The ops that close a finished landing's records: its grant's transition, and the volume's new base when
/// the landing made a scratch volume an overlay.
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
fn degradation_of(degradation: &Degradation) -> LandingDegradation {
  match degradation {
    Degradation::NoExchange { widest_window_ns } => LandingDegradation::NoExchange {
      widest_window_ns: *widest_window_ns,
    },
    Degradation::BarriersOnly => LandingDegradation::BarriersOnly,
    // The code in the variant of its numbering (A-539).
    Degradation::Crashed {
      code: slates_vfs::host::OsCode::Errno(errno),
    } => LandingDegradation::Crashed { errno: *errno },
    Degradation::Crashed {
      code: slates_vfs::host::OsCode::Win32(code),
    } => LandingDegradation::CrashedWin32 { code: *code },
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
fn answer_of(error: HostError) -> HostAnswer {
  match error {
    HostError::NotFound => HostAnswer::NotFound,
    HostError::NotDirectory => HostAnswer::NotDirectory,
    HostError::NotFile => HostAnswer::NotFile,
    HostError::StaleHandle => HostAnswer::StaleHandle,
    // The code in the variant of its numbering (A-539); a path on a mount this daemon serves carries the platform's
    // deadlock-avoided code (A-227).
    HostError::Exists(code)
    | HostError::NoSpace(code)
    | HostError::Unsupported(code)
    | HostError::Unavailable(code)
    | HostError::OwnMount(code) => match code {
      slates_vfs::host::OsCode::Win32(code) => HostAnswer::Win32 { code },
      slates_vfs::host::OsCode::Errno(errno) => HostAnswer::Errno { errno },
    },
    // A landing writes a directory host, which never answers a pending fetch (an S3 base's, A-524); were one to, it is
    // a refusal no errno names.
    HostError::Pending => HostAnswer::Errno { errno: 0 },
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
/// Refuses `Forbidden` — counted `grant_by_consumer` — on a channel bound to a consumer (an agent's session), before
/// anything else and whatever its proof (A-285): an agent never approves its own landing, as it never approves its own
/// merge (`submit_approved`) or sets a policy (`set_policy_verb`); the approval is the launcher's or its harness's.
pub fn grant_verb(
  state: &mut ShardState,
  principal: &Principal,
  landing_id: u64,
  manifest: [u8; 32],
  scope: GrantScope,
  term_ns: u64,
  proof: [u8; 32],
) -> ReplyBody {
  // A-285: an agent never approves its own landing, even holding a genuine proof.
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("grant_by_consumer", 1);
    return crate::verbs::forbidden("grant");
  }
  let Some(awaiting) = state.landing.awaiting.get(&landing_id).cloned() else {
    return crate::verbs::refused(Refusal::NotFound);
  };
  if !proven_by_launcher(
    state,
    |secret| grant_proof(secret, landing_id, &manifest, scope, term_ns),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return crate::verbs::refused(Refusal::GrantIssuerUnverified);
  }
  if awaiting.manifest != manifest {
    return crate::verbs::refused(Refusal::GrantMismatch);
  }
  match issue_grant(
    state,
    principal,
    landing_id,
    (scope, term_ns),
    Issuer::Human,
  ) {
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

/// Whether two fixed-size secrets are equal, visiting every byte whatever the first difference (no early
/// exit), so the comparison's time does not depend on how much of a forged secret happened to match. Used
/// for the 32-byte grant proof (above) and the 16-byte mount capability token (`crate::nfs`); the const
/// length couples both operands to one size, so there is no length branch to leak either.
pub(crate) fn constant_time_eq<const N: usize>(a: &[u8; N], b: &[u8; N]) -> bool {
  let mut difference = 0u8;
  for (x, y) in a.iter().zip(b.iter()) {
    difference |= x ^ y;
  }
  difference == 0
}

/// Who issues a grant (A-285, R10): the launcher's or the harness's approval, proven under the issuer secret
/// ([`grant_verb`]), or the volume's bypass policy, which that same authority set ([`set_policy_verb`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Issuer {
  /// An approval proven under the issuer secret, recorded as the command line's surface.
  Human,
  /// The daemon itself, under the volume's bypass policy, recorded as the policy's surface.
  Policy,
}

/// Format: the domain tag of a policy proof, so it never verifies as a grant, enrollment or revocation proof.
const POLICY_DOMAIN: &[u8] = b"slates.policy";

/// The proof a `SetPolicy` request carries (A-285): the keyed hash, under the launcher's secret, of the policy domain
/// tag, the subject and the policy in their wire encodings, and the policy version the setter read. Only the launcher
/// or the harness can make one; an agent's consumer channel never holds the secret. The version makes a captured proof
/// good for one change: once the policy has moved past it, the proof is refused `PolicyChanged`.
pub fn policy_proof(
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  subject: &PolicySubject,
  policy: &WritePolicy,
  version: u64,
) -> [u8; 32] {
  let mut bytes = Vec::new();
  slates_wire::Wire::encode(subject, &mut bytes);
  slates_wire::Wire::encode(policy, &mut bytes);
  bytes.extend_from_slice(&version.to_le_bytes());
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(POLICY_DOMAIN);
  hasher.update(&bytes);
  *hasher.finalize().as_bytes()
}

/// Whether `proof` is the launcher's (A-285, R10): `make`'s proof under the secret this daemon minted at its start (the
/// issuer, held by the anchor's tree), or under the launcher secret its starter handed the anchor (kept by the launcher
/// across its own restarts and the daemon's), compared in constant time. The same formula and domain under either:
/// the launcher's authority is one, however it is held.
pub(crate) fn proven_by_launcher(
  state: &ShardState,
  make: impl Fn(&[u8; slates_anchor::layout::ISSUER_SECRET_BYTES]) -> [u8; 32],
  proof: &[u8; 32],
) -> bool {
  let by_issuer = constant_time_eq(&make(&state.issuer_secret), proof);
  let by_launcher = state
    .launcher_secret
    .as_ref()
    .is_some_and(|secret| constant_time_eq(&make(secret), proof));
  by_issuer | by_launcher
}

/// Format: the domain tag of a merge approval's proof.
const MERGE_APPROVAL_DOMAIN: &[u8] = b"slates.merge-approval";

/// The proof a `SubmitApproved` request carries (A-285): the keyed hash, under the issuer secret, of the merge approval
/// domain tag, the work volume and the approved increment's identity.
pub fn merge_approval_proof(
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  work: VolumeId,
  identity: &[u8; 32],
) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(MERGE_APPROVAL_DOMAIN);
  hasher.update(&work.bytes);
  hasher.update(identity);
  *hasher.finalize().as_bytes()
}

/// Serves `SetPolicy` (A-285): refused `Forbidden` on a consumer-bound channel, whatever its proof, so an agent never
/// sets (and so never raises) its own policy, counted `policy_set_by_consumer`; `GrantIssuerUnverified` when the proof
/// does not verify; a volume's policy recorded durably in its record. A consumer's policy is set on the channel
/// (`verbs::set_consumer_policy_on_channel`), which reaches every shard's bound slots.
pub fn set_policy_verb(
  state: &mut ShardState,
  principal: &Principal,
  (subject, policy, version): (PolicySubject, WritePolicy, u64),
  proof: [u8; 32],
) -> ReplyBody {
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("policy_set_by_consumer", 1);
    return crate::verbs::forbidden("set_policy");
  }
  if !proven_by_launcher(
    state,
    |secret| policy_proof(secret, &subject, &policy, version),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return crate::verbs::refused(Refusal::GrantIssuerUnverified);
  }
  let PolicySubject::Volume { volume } = subject else {
    return crate::verbs::refused(Refusal::BadRequest {
      reason: "a consumer's policy is set on its channel".to_owned(),
    });
  };
  let current = match policy_record(state, volume) {
    Ok(record) => record.policy.write_version,
    Err(reply) => return *reply,
  };
  // The setter's version is the one the policy is at (A-285): a proof made for an earlier state of the policy, a
  // replayed or a raced one, changes nothing.
  if version != current {
    state.count("policy_changed", 1);
    return crate::verbs::refused(Refusal::PolicyChanged { current });
  }
  let now = state.clock.monotonic_ns();
  let op = Op::VolumeWritePolicySet {
    id: crate::verbs::to_db_volume(volume),
    policy: db_policy(policy),
  };
  match state.db.mutate(&mut state.segment, &op, now) {
    Ok(_) => ReplyBody::PolicySet,
    Err(e) => crate::verbs::refused(crate::error::refusal_of_db(&e)),
  }
}

/// Serves `Policy` (A-285): a volume's write policy and its version, `None` while none is set (merges at once, lands
/// under approval). Any principal may read it: it holds only the modes the launcher set, which an agent meets anyway in
/// its own merges' and landings' answers, and a launcher setting a policy on an agent's own volume must read the
/// version it replaces. A consumer's policy is read on the channel (`verbs::consumer_policy_on_channel`).
pub fn policy_verb(state: &mut ShardState, subject: PolicySubject) -> ReplyBody {
  let PolicySubject::Volume { volume } = subject else {
    return crate::verbs::refused(Refusal::BadRequest {
      reason: "a consumer's policy is read on its channel".to_owned(),
    });
  };
  let record = match policy_record(state, volume) {
    Ok(record) => record,
    Err(reply) => return *reply,
  };
  ReplyBody::Policy {
    policy: record.policy.write.map(wire_policy),
    version: record.policy.write_version,
    signers: record.policy.signers,
  }
}

/// Format: the domain tag of a signer set's proof (A-321).
const SIGNERS_DOMAIN: &[u8] = b"slates.signers";

/// The proof a `SetSigners` request carries (A-321): the keyed hash, under the launcher's secret, of the signers domain
/// tag, the volume, the number of keys and each key with its length, and the policy version the setter read, so a
/// captured proof sets that one set once.
pub fn signers_proof(
  secret: &[u8; slates_anchor::layout::ISSUER_SECRET_BYTES],
  volume: VolumeId,
  keys: &[Vec<u8>],
  version: u64,
) -> [u8; 32] {
  let length = |count: usize| u64::try_from(count).unwrap_or(u64::MAX).to_le_bytes();
  let mut hasher = blake3::Hasher::new_keyed(secret);
  hasher.update(SIGNERS_DOMAIN);
  hasher.update(&volume.bytes);
  hasher.update(&length(keys.len()));
  for key in keys {
    hasher.update(&length(key.len()));
    hasher.update(key);
  }
  hasher.update(&version.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// Serves `SetSigners` (A-321): refused as `SetPolicy` is (`Forbidden` on a consumer-bound channel whatever its proof,
/// counted `signers_set_by_consumer`; `GrantIssuerUnverified` without the launcher's proof; `PolicyChanged` from a
/// version the policy has moved past), and `BadRequest` for a key slates does not verify; otherwise the set recorded
/// durably in the volume's record, a key named twice kept once, and the policy's version moved.
pub fn set_signers_verb(
  state: &mut ShardState,
  principal: &Principal,
  (volume, keys, version): (VolumeId, Vec<Vec<u8>>, u64),
  proof: [u8; 32],
) -> ReplyBody {
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("signers_set_by_consumer", 1);
    return crate::verbs::forbidden("set_signers");
  }
  if !proven_by_launcher(
    state,
    |secret| signers_proof(secret, volume, &keys, version),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return crate::verbs::refused(Refusal::GrantIssuerUnverified);
  }
  let mut set: Vec<Vec<u8>> = Vec::with_capacity(keys.len());
  for key in keys {
    if crate::signed_work::TrustedKey::from_wire(key.clone()).is_err() {
      return crate::verbs::refused(Refusal::BadRequest {
        reason:
          "a trusted signer is an ssh-ed25519 or sk-ssh-ed25519@openssh.com key in SSH wire form"
            .to_owned(),
      });
    }
    if !set.contains(&key) {
      set.push(key);
    }
  }
  let current = match policy_record(state, volume) {
    Ok(record) => record.policy.write_version,
    Err(reply) => return *reply,
  };
  if version != current {
    state.count("policy_changed", 1);
    return crate::verbs::refused(Refusal::PolicyChanged { current });
  }
  let now = state.clock.monotonic_ns();
  let op = Op::VolumeSignersSet {
    id: crate::verbs::to_db_volume(volume),
    keys: set,
  };
  match state.db.mutate(&mut state.segment, &op, now) {
    Ok(_) => ReplyBody::SignersSet,
    Err(e) => crate::verbs::refused(crate::error::refusal_of_db(&e)),
  }
}

/// The catalog record a policy is set on or read from (A-285): any volume this shard holds — a store-backed volume, a
/// green or a work, which holds no store slot — refused `NotFound` for one it does not and `Destroying` for one going.
fn policy_record(
  state: &ShardState,
  volume: VolumeId,
) -> Result<slates_db::catalog::VolumeRecord, Box<ReplyBody>> {
  let record = state
    .db
    .partition()
    .volume(crate::verbs::to_db_volume(volume))
    .cloned()
    .ok_or_else(|| Box::new(crate::verbs::refused(Refusal::NotFound)))?;
  if matches!(
    record.state,
    slates_db::catalog::VolumeState::Destroying | slates_db::catalog::VolumeState::Destroyed
  ) {
    return Err(Box::new(crate::verbs::refused(Refusal::Destroying)));
  }
  Ok(record)
}

/// A volume's landing mode (A-285): its policy's, or ask while none is set.
fn land_mode(record: &slates_db::catalog::VolumeRecord) -> slates_db::catalog::WriteMode {
  record
    .policy
    .write
    .map_or(slates_db::catalog::WriteMode::Ask, |policy| policy.land)
}

/// A wire policy as the catalog records it.
pub(crate) fn db_policy(policy: WritePolicy) -> slates_db::catalog::WritePolicy {
  slates_db::catalog::WritePolicy {
    merge: db_mode(policy.merge),
    land: db_mode(policy.land),
    require_signed_merges: policy.require_signed_merges,
  }
}

/// A wire mode as the catalog records it.
fn db_mode(mode: WriteMode) -> slates_db::catalog::WriteMode {
  match mode {
    WriteMode::Bypass => slates_db::catalog::WriteMode::Bypass,
    WriteMode::Ask => slates_db::catalog::WriteMode::Ask,
    WriteMode::Forbid => slates_db::catalog::WriteMode::Forbid,
  }
}

/// A recorded policy as the wire carries it.
pub(crate) fn wire_policy(policy: slates_db::catalog::WritePolicy) -> WritePolicy {
  let mode = |mode| match mode {
    slates_db::catalog::WriteMode::Bypass => WriteMode::Bypass,
    slates_db::catalog::WriteMode::Ask => WriteMode::Ask,
    slates_db::catalog::WriteMode::Forbid => WriteMode::Forbid,
  };
  WritePolicy {
    merge: mode(policy.merge),
    land: mode(policy.land),
    require_signed_merges: policy.require_signed_merges,
  }
}

/// Issues a grant for a presented landing, binding its manifest (§4.15 step 3). Reached through [`grant_verb`],
/// whose proof of the launcher's authority has verified, or a bypass presentation, under the policy that authority set
/// (A-285); `issuer` says which, for the record. The grant is issued into the runtime grants and persisted as a
/// `GrantRecord`; the landing's record gains the grant. Returns the grant id.
pub fn issue_grant(
  state: &mut ShardState,
  principal: &Principal,
  landing_id: u64,
  (scope, term_ns): (GrantScope, u64),
  issuer: Issuer,
) -> Result<u64, Refusal> {
  let (surface, grant_surface) = match issuer {
    Issuer::Human => (Surface::Cli, GrantSurface::Cli),
    Issuer::Policy => (Surface::Policy, GrantSurface::Policy),
  };
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
      surface,
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
    surface: grant_surface,
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
    signature: None,
  };
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::GrantIssued { record }, now)
  {
    // The issue was refused durably and nothing was applied, so the grant leaves this shard's table too (A-266):
    // nothing lands under a grant the caller was told was not made. A commit refused later in the atom rolls the
    // record back, and `reconcile_unpublished_effects` takes the grant out then.
    state.landing.grants.end(id);
    return Err(crate::error::refusal_of_db(&e));
  }
  write_audit(state, vec![audit], now).map_err(|e| crate::error::refusal_of_db(&e))?;
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
        GrantSurface::Policy => Surface::Policy,
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
  // The table holds live grants only (A-266): an ended grant's id is known by the partition's mark alone.
  let issued_through = state.db.partition().grants_issued_through();
  state.landing.grants.restore_issued_through(issued_through);
}

/// The caller's grants, from the durable records.
pub(crate) fn grants_part(
  state: &ShardState,
  principal: &Principal,
  after: Option<u64>,
) -> crate::verbs::PagePart<GrantSummary> {
  let grants = state
    .db
    .partition()
    .grants_of_after(principal, after)
    .map(|g| GrantSummary {
      id: g.id,
      volume: to_wire_volume(g.volume),
      target: g.target.clone(),
      manifest: g.manifest,
      scope: wire_grant_scope(&g.scope),
      state: grant_state_name(g.state),
      grant_state: wire_grant_state(g.state),
    });
  // `grants_of_after` walks the partition's grants in id order from the cursor, lazily, so the first that fit are the
  // page's and the walk stops there.
  let (items, more) = crate::verbs::fitting(grants, grants_page_room());
  crate::verbs::PagePart { items, more }
}

/// The bytes a grants page's summaries may take: one reply chunk less the page's own framing, its cursor included.
pub(crate) fn grants_page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::Grants {
    grants: Vec::new(),
    next: Some(u64::MAX),
  };
  chunk.saturating_sub(slates_ipc::protocol::framed_len(&empty))
}

/// The longest target `skeleton`'s presentation could carry when it does not fit one reply chunk (§4.3; A-267): a
/// presentation's reply carries the target in the statement a signature over it signs (A-321), so a reply past the
/// chunk is short of exactly its overflow in target bytes. `None` when the reply fits, conflicts aside (they are
/// carried as far as the room left holds and counted past it).
fn presentation_overflow(skeleton: &ReplyBody, target_len: usize) -> Option<u64> {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let overflow = slates_ipc::protocol::framed_len(skeleton).checked_sub(chunk)?;
  (overflow > 0).then(|| u64::try_from(target_len.saturating_sub(overflow)).unwrap_or(u64::MAX))
}

/// Derived: the longest landing target, in bytes, a grant's summary on one grants page holds alone (§4.15, §4.3): the
/// page's room less a summary with an empty target in its longest state. A grant is issued for a presented landing and
/// listed with its target, so a presentation past it is refused `TargetUnavailable` before anything is made. A `land`
/// request carries a target of about 4,046 bytes, past what a page held beside a grant's other fields (2026-11-08).
pub fn longest_grantable_target() -> usize {
  let longest_state = [
    DbGrantState::Issued,
    DbGrantState::Consumed,
    DbGrantState::Expired,
    DbGrantState::Revoked,
  ]
  .into_iter()
  .map(grant_state_name)
  .max_by_key(String::len)
  .unwrap_or_default();
  let untargeted = GrantSummary {
    id: u64::MAX,
    volume: VolumeId::default(),
    target: String::new(),
    manifest: [u8::MAX; 32],
    scope: slates_ipc::protocol::GrantScope::Session,
    state: longest_state,
    grant_state: slates_ipc::protocol::GrantState::Issued,
  };
  grants_page_room().saturating_sub(crate::verbs::encoded_len(&untargeted))
}

/// The grants page the shards' `parts` make: their grants in id order, as many as one reply chunk holds, and the
/// cursor past the last when any part, or the merge, left more.
pub(crate) fn grants_page(parts: Vec<crate::verbs::PagePart<GrantSummary>>) -> ReplyBody {
  let (grants, more) = crate::verbs::merged_page(parts, grants_page_room(), |grant| grant.id);
  let next = more.then(|| grants.last().map(|grant| grant.id)).flatten();
  ReplyBody::Grants { grants, next }
}

/// One page of the audit (§4.15, A-265), answered on the control shard, whose partition holds every record: the
/// retained records from sequence `since` on, as many as one reply chunk holds, and whether more follow.
pub fn audit_verb(state: &ShardState, since: u64) -> ReplyBody {
  let room = audit_page_room();
  let mut used = 0usize;
  let mut records = Vec::new();
  let mut more = false;
  // A seek to `since`, so a page costs what it carries, never the retained window (A-267).
  for record in state.db.partition().audit_from(since) {
    let entry = AuditEntry {
      seq: record.seq,
      at_ns: record.at_ns,
      kind: audit_kind_name(record.kind),
      grant: record.grant,
      landing: record.landing,
      manifest: record.manifest,
      outcome: record.outcome.map(landing_state_name),
      audit_kind: wire_audit_kind(record.kind),
      stage: record.outcome.map(wire_landing_stage),
      // A record of a landing as a whole — its presentation, a granted run's plan, its finish — says how the landing's
      // signature was judged (A-321), unsigned when it carried none; no other record judges one, nor one that names no
      // manifest, which had no statement to judge. A verdict this build names no code for (a newer build's record) reads
      // as none.
      verification: (matches!(
        record.kind,
        slates_db::catalog::AuditKind::LandingPlanned
          | slates_db::catalog::AuditKind::LandingFinished
      ) && record.manifest.is_some())
      .then(|| crate::signed_work::of_record(record.signature))
      .flatten(),
    };
    let bytes = crate::verbs::encoded_len(&entry);
    if !records.is_empty() && used.saturating_add(bytes) > room {
      more = true;
      break;
    }
    used = used.saturating_add(bytes);
    records.push(entry);
  }
  ReplyBody::Audit { records, more }
}

/// The bytes an audit page's records may take: one reply chunk less the page's own framing (§4.3). A record is a fixed
/// kind name, ids, a manifest hash and a terminal state's name, so every record fits a page alone.
pub(crate) fn audit_page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::Audit {
    records: Vec::new(),
    more: true,
  };
  chunk.saturating_sub(slates_ipc::protocol::framed_len(&empty))
}

/// Format: the partition that holds the audit (§4.15 "written through the control shard", §4.8's one `AuditLog`):
/// the control shard's, the first of the daemon's shards.
pub(crate) const AUDIT_PARTITION: u16 = 0;

/// Writes `records` to the audit (A-265): applied in the current atom when this shard is the control shard, else owed
/// to it in the same atom (`Op::AuditOwed`, the partition's outbox), so a record is durable with the effect it records
/// and a crash before its batch arrives loses nothing (AC-2.10). The atom's caller sends what is owed once the atom
/// commits ([`send_owed_audit`], [`append_owed_audit`]), and a record owed arms the look that sends it again unless it
/// is acknowledged a retry window on ([`arm_owed_audit_look`], A-554); a refused atom's owed records roll back with it,
/// and the look then finds nothing owed.
fn write_audit(
  state: &mut ShardState,
  records: Vec<DbAuditRecord>,
  now: u64,
) -> Result<(), slates_db::DbError> {
  let owed = state.partition != AUDIT_PARTITION;
  for record in records {
    let op = if owed {
      Op::AuditOwed { record }
    } else {
      Op::AuditAppended { record }
    };
    state.db.mutate(&mut state.segment, &op, now)?;
  }
  if owed {
    arm_owed_audit_look(state, now);
  }
  Ok(())
}

/// Arms the deadline keeper's look at what this partition owes the audit (A-554): a retry window from `now`
/// (`deadlines::retry_window_ns`), the outbox's front noted for the look to compare, unless a look is armed already or
/// nothing is owed. A record owed when the shard starts (a batch a crash or a restart cut off) is looked at a window
/// into the start, as the reap loop's first tick did.
pub(crate) fn arm_owed_audit_look(state: &mut ShardState, now: u64) {
  if state.landing.audit_look_at.is_some() {
    return;
  }
  let Some(front) = state
    .db
    .partition()
    .audit_outbox()
    .next()
    .map(|record| record.seq)
  else {
    return;
  };
  let at = now.saturating_add(crate::deadlines::retry_window_ns());
  state.landing.outbox_front_seen = Some(front);
  state.landing.audit_look_at = Some(at);
  crate::deadlines::noted(state, at);
}

/// What this shard's partition owes the control partition (A-265): the partition, the crossing sequence of its oldest
/// owed record, and every owed record from it on, so a batch that was lost is carried by the next. `None` when nothing
/// is owed, on the control shard, or while a test withholds the crossing.
fn owed_batch(state: &ShardState) -> Option<(u16, u64, Vec<DbAuditRecord>)> {
  if state.partition == AUDIT_PARTITION || state.landing.crossing_withheld {
    return None;
  }
  let records: Vec<DbAuditRecord> = state.db.partition().audit_outbox().cloned().collect();
  let first = records.first()?.seq;
  Some((state.partition, first, records))
}

/// Appends `origin`'s batch of owed audit records on the control shard (A-265): the records past what `origin` has
/// delivered, in one record (`Op::AuditBatchAppended`), whose apply passes over any it already holds, so a batch sent
/// again after a crash or a lost acknowledgement appends nothing twice. The crossing sequence `origin` has delivered
/// through when it committed, which the caller acknowledges; `None`, counted, when refused.
fn append_audit_batch(
  state: &mut ShardState,
  (origin, first, records): (u16, u64, Vec<DbAuditRecord>),
) -> Option<u64> {
  let from = state.db.partition().audit_received(origin);
  let end = first.saturating_add(u64::try_from(records.len()).unwrap_or(u64::MAX));
  if end <= from {
    return Some(from);
  }
  // The records the control partition holds already are not logged again.
  let skip = usize::try_from(from.saturating_sub(first)).unwrap_or(usize::MAX);
  let op = Op::AuditBatchAppended {
    origin,
    first: first.max(from),
    records: records.into_iter().skip(skip).collect(),
  };
  let now = state.clock.monotonic_ns();
  match state.db.mutate(&mut state.segment, &op, now) {
    Ok(_) => Some(end),
    Err(_) => {
      state.count(AUDIT_UNAPPENDED, 1);
      None
    }
  }
}

/// Records on this shard's partition that the control partition appended its owed records before crossing sequence
/// `through` (A-265): they leave the outbox. Nothing when the outbox holds none of them (an older or a repeated
/// acknowledgement). A refused record is counted, and the records stay owed: the next batch carries them, and the
/// control partition appends none of them twice.
fn record_delivered(state: &mut ShardState, through: u64) {
  if state.landing.acknowledgement_withheld {
    return;
  }
  let owed = state
    .db
    .partition()
    .audit_outbox()
    .next()
    .is_some_and(|record| record.seq < through);
  if !owed {
    return;
  }
  let now = state.clock.monotonic_ns();
  if state
    .db
    .mutate(&mut state.segment, &Op::AuditDelivered { through }, now)
    .is_err()
  {
    state.count(AUDIT_UNACKNOWLEDGED, 1);
  }
}

/// Sends what this shard's partition owes the audit to the control shard (A-265), and the control shard's
/// acknowledgement back, without waiting: after a sync verb's transaction (`verbs::run_recorded`), which cannot wait, and
/// from the deadline keeper's look ([`look_at_owed_audit`]). Nothing when nothing is owed. A batch that cannot be sent,
/// or is refused, is counted ([`AUDIT_UNAPPENDED`]) and stays owed, durable, for the next send.
pub(crate) fn send_owed_audit(state: &mut ShardState) {
  let Some(batch) = owed_batch(state) else {
    return;
  };
  let Some(control) = crate::verbs::shard_of_partition(state, AUDIT_PARTITION) else {
    state.count(AUDIT_UNAPPENDED, 1);
    return;
  };
  let origin = state.shard;
  crate::xshard::run_on_waiting(
    origin,
    control,
    move |s| {
      if let Some(through) = append_audit_batch(s, batch) {
        crate::xshard::run_on_waiting(
          control,
          origin,
          move |o| record_delivered(o, through),
          AUDIT_UNACKNOWLEDGED,
        );
      }
    },
    AUDIT_UNAPPENDED,
  );
}

/// The deadline keeper's look at what this partition owes the audit, at its armed deadline (A-265, A-554): an outbox
/// whose front is where the look was armed or last taken, a retry window ago, is sent again, since its batch was lost
/// (a crash or a restart between the owning atom's commit and the batch, an unanswered wait, a refused append) or is
/// slow, and the control partition appends nothing twice. While anything is owed the look is armed again a window on;
/// once nothing is, it is not, so a shard that owes nothing is never woken for the audit. Until A-554 the reap loop's
/// tick looked once a window, so a record owed just after a look was sent again nearly two windows on, and the first
/// tick after a start sent again what had been sent a moment before.
pub(crate) fn look_at_owed_audit(state: &mut ShardState, now: u64) {
  state.landing.audit_look_at = None;
  let seen = state.landing.outbox_front_seen.take();
  let Some(front) = state
    .db
    .partition()
    .audit_outbox()
    .next()
    .map(|record| record.seq)
  else {
    return;
  };
  if seen == Some(front) && !state.landing.crossing_withheld {
    state.count(AUDIT_RESENT, 1);
    send_owed_audit(state);
  }
  arm_owed_audit_look(state, now);
}

/// Sends what this shard's partition owes the audit to the control shard and waits for it (A-265): a presentation's or a
/// granted landing's completion owed it, and its reply waits on it, so the caller's next `audit` reads it. Waited in the
/// control shard's own time (A-432): `budget_ns`, then on while the shard works or waits for a CPU, up to `budget_ns` of
/// its CPU; it waited `budget_ns` on the wall, so a busy control shard left the record owed and the reply went without
/// it. Whether everything owed when it was called is appended; counted when not, and then still owed, durable, for the
/// next send.
async fn append_owed_audit(shard: u16, budget_ns: u64) -> bool {
  let Some(Some(batch)) = crate::state::with_state(|s| owed_batch(s)) else {
    return true;
  };
  let control =
    crate::state::with_state(|s| crate::verbs::shard_of_partition(s, AUDIT_PARTITION)).flatten();
  let answered = match control {
    Some(control) => {
      crate::xshard::call_in_shard_time(
        shard,
        control,
        move |s| append_audit_batch(s, batch),
        budget_ns,
      )
      .await
    }
    None => None,
  };
  match answered.flatten() {
    Some(through) => {
      let _ = crate::state::with_state(|s| record_delivered(s, through));
      true
    }
    None => {
      // A batch the control shard refused is counted there; one that never reached it, here.
      if answered.is_none() {
        let _ = crate::state::with_state(|s| s.count(AUDIT_UNAPPENDED, 1));
      }
      false
    }
  }
}

/// Format: the landing-plane counter of an acknowledgement of appended audit records that this shard could not record
/// (A-265): the records stay owed, and the next batch carries them; the control partition appends none twice.
pub(crate) const AUDIT_UNACKNOWLEDGED: &str = "landing.audit_unacknowledged";

/// Format: the landing-plane counter of the deadline keeper's sends of owed audit records whose batch had not been
/// acknowledged a retry window after it was sent (A-265, A-554): a lost batch, a restart, or a slow control shard.
pub const AUDIT_RESENT: &str = "landing.audit_resent";

/// Format: the landing-plane counter of an audit batch that did not reach the control partition (A-265): refused at
/// its channel, unanswered within its budget, or refused by its transaction.
pub(crate) const AUDIT_UNAPPENDED: &str = "landing.audit_unappended";

fn to_wire_volume(id: DbVolumeId) -> VolumeId {
  VolumeId { bytes: id.bytes }
}

fn session_of(principal: &Principal) -> u64 {
  match principal {
    Principal::Uid { uid, .. } => u64::from(*uid),
    // A consumer's session is its own, not its account's: two consumers under one uid never share a
    // session-scoped grant (§4.13 "distinct consumers sharing a uid cannot use each other's grant rights").
    Principal::Consumer { consumer, .. } => *consumer,
    Principal::Sid { .. } | Principal::Certificate { .. } => 0,
  }
}

fn to_land_filter(filter: &slates_ipc::protocol::Filter) -> Filter {
  Filter {
    include: filter.include.iter().map(|s| s.as_str().into()).collect(),
    exclude: filter.exclude.iter().map(|s| s.as_str().into()).collect(),
  }
}

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
    actions: tally_of(manifest),
  }
}

/// A manifest's entries per action, typed (A-299): counted from its entries' actions, one exhaustive match, so a new
/// action does not compile until it is counted.
fn tally_of(manifest: &Manifest) -> slates_ipc::protocol::ActionTally {
  use slates_land::manifest::Action;
  let mut tally = slates_ipc::protocol::ActionTally::default();
  for entry in &manifest.entries {
    let count = match &entry.action {
      Action::Create => &mut tally.create,
      Action::Replace => &mut tally.replace,
      Action::Delete => &mut tally.delete,
      Action::Rename { .. } => &mut tally.rename,
      Action::Mkdir => &mut tally.mkdir,
      Action::Rmdir => &mut tally.rmdir,
      Action::Symlink { .. } => &mut tally.symlink,
      Action::Clear => &mut tally.clear,
      Action::Chmod { .. } | Action::ChmodDir { .. } => &mut tally.chmod,
    };
    *count = count.saturating_add(1);
  }
  tally
}

fn target_refusal(refusal: &TargetRefusal, target: &str) -> Refusal {
  use slates_ipc::protocol::TargetRefusal as Typed;
  let typed = match refusal {
    TargetRefusal::OwnMount => {
      return Refusal::OwnMount {
        path: target.to_owned(),
        used_as: slates_ipc::protocol::OwnMountUse::LandingTarget,
      };
    }
    TargetRefusal::NotAbsolute => Typed::NotAbsolute,
    TargetRefusal::EscapesTarget => Typed::Escapes,
    TargetRefusal::TargetNotOwned => Typed::OwnedByAnother { owner: None },
    TargetRefusal::Unavailable(e) => crate::error::target_of_host(e),
  };
  crate::error::target(typed)
}

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

/// A landing's state on the wire, typed (A-299).
fn wire_landing_stage(state: DbLandingState) -> slates_ipc::protocol::LandingStage {
  use slates_ipc::protocol::LandingStage as Stage;
  match state {
    DbLandingState::Planning => Stage::Planning,
    DbLandingState::AwaitingGrant => Stage::AwaitingGrant,
    DbLandingState::Validating => Stage::Validating,
    DbLandingState::Writing => Stage::Writing,
    DbLandingState::Syncing => Stage::Syncing,
    DbLandingState::Advancing => Stage::Advancing,
    DbLandingState::Done => Stage::Done,
    DbLandingState::Partial => Stage::Partial,
    DbLandingState::Refused => Stage::Refused,
    DbLandingState::Aborted => Stage::Aborted,
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

/// One of the engine's audit records as the durable log keeps it. A record of the landing as a whole — planned or
/// finished — carries how its signature was judged (A-321), as `audit_verb` reads it back: a granted run's own planned
/// record is mirrored here with its finished one, and read unsigned it would contradict the landing it planned.
fn db_audit(
  audit: &AuditRecord,
  principal: &Principal,
  verification: &slates_ipc::protocol::Verification,
) -> DbAuditRecord {
  DbAuditRecord {
    seq: 0,
    at_ns: audit.at_ns,
    kind: db_audit_kind(audit.kind),
    principal: principal.clone(),
    grant: audit.grant.map(|g| g.0),
    landing: Some(audit.landing),
    manifest: Some(audit.manifest),
    outcome: audit.outcome.map(|s| db_landing_state(&s)),
    signature: matches!(
      audit.kind,
      AuditKind::LandingPlanned | AuditKind::LandingFinished
    )
    .then(|| crate::signed_work::to_record(verification))
    .flatten(),
  }
}

fn db_audit_kind(kind: AuditKind) -> DbAuditKind {
  match kind {
    AuditKind::LandingPlanned => DbAuditKind::LandingPlanned,
    AuditKind::LandingValidated => DbAuditKind::LandingValidated,
    AuditKind::EntryWritten => DbAuditKind::EntryWritten,
    AuditKind::EntryRefused => DbAuditKind::EntryRefused,
    AuditKind::LandingFinished => DbAuditKind::LandingFinished,
  }
}

/// An audit record's kind on the wire, typed (A-299).
fn wire_audit_kind(kind: DbAuditKind) -> slates_ipc::protocol::AuditKind {
  use slates_ipc::protocol::AuditKind as Kind;
  match kind {
    DbAuditKind::GrantIssued => Kind::GrantIssued,
    DbAuditKind::GrantRevoked => Kind::GrantRevoked,
    DbAuditKind::LandingPlanned => Kind::LandingPlanned,
    DbAuditKind::LandingValidated => Kind::LandingValidated,
    DbAuditKind::EntryWritten => Kind::EntryWritten,
    DbAuditKind::EntryRefused => Kind::EntryRefused,
    DbAuditKind::LandingFinished => Kind::LandingFinished,
    DbAuditKind::GrantExpired => Kind::GrantExpired,
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
    DbAuditKind::GrantExpired => "grant_expired",
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

/// A grant's state on the wire, typed (A-299).
fn wire_grant_state(state: DbGrantState) -> slates_ipc::protocol::GrantState {
  use slates_ipc::protocol::GrantState as Wire;
  match state {
    DbGrantState::Issued => Wire::Issued,
    DbGrantState::Consumed => Wire::Consumed,
    DbGrantState::Expired => Wire::Expired,
    DbGrantState::Revoked => Wire::Revoked,
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

#[cfg(test)]
mod tests {
  use super::{
    AuditKind, AuditRecord, DbLandingState, ENROLL_DOMAIN, REVOKE_DOMAIN, SpanObserver,
    attest_proof, enroll_proof, refused_state, release_target_lease, revoke_proof,
    take_target_lease,
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

  /// §4.15's state machine (`→ Refused`, `→ Aborted`): a refused granted landing's end names what it left on the disk.
  /// Do: decide the end of an attempt whose ring records hold its plan, its validation and an entry it refused, and of
  /// one whose records hold an entry it wrote as well. Expect `Refused` for the first, nothing written, and `Aborted` for
  /// the second, its written entry on the disk for a resume to continue.
  #[test]
  fn a_refused_attempts_end_is_aborted_once_it_wrote_an_entry() {
    let record = |kind| AuditRecord {
      seq: 0,
      at_ns: 0,
      kind,
      grant: None,
      landing: 1,
      manifest: [0; 32],
      outcome: None,
    };
    let refused_whole = [
      record(AuditKind::LandingPlanned),
      record(AuditKind::LandingValidated),
      record(AuditKind::EntryRefused),
    ];
    assert_eq!(refused_state(&refused_whole), DbLandingState::Refused);
    let wrote_one = [
      record(AuditKind::LandingPlanned),
      record(AuditKind::LandingValidated),
      record(AuditKind::EntryWritten),
      record(AuditKind::EntryRefused),
    ];
    assert_eq!(refused_state(&wrote_one), DbLandingState::Aborted);
  }

  /// §4.15 step 4 (AUD-29-03): the control shard's target lease has one holder until it is released or its
  /// term ends. Do: attempt A takes a target; attempt B takes it; B releases it; A releases it and C takes it.
  /// Expect: B's take is refused `LandingLeaseHeld` naming A; another target is its own lease; B's release frees
  /// nothing; after A's release C holds it under a greater generation; the status report counts each record and
  /// none once released. (A's own second take renews it: `a_holder_renews_its_own_live_lease_and_no_other_holder_can`.)
  #[test]
  fn a_target_lease_has_one_holder_until_its_release_or_its_term() {
    crate::daemon::audit_on_shard(|state| {
      let deadline = far(state);
      let a = take_target_lease(state, target(2), 10, LONG_TERM_NS, deadline).unwrap();
      assert_eq!(
        take_target_lease(state, target(2), 11, LONG_TERM_NS, deadline),
        Err(Refusal::LandingLeaseHeld { holder: 10 }),
        "another attempt"
      );
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

  /// §4.15 "The lease keepalive" (AUD-29-25): a running landing renews its target lease between slices by taking it
  /// again as its own holder once half its term has passed, while the lease still holds. Do: take a target for one
  /// attempt; take it again for the same attempt before its term ends; then take it for another attempt. Expect: the
  /// second take renews it (a later generation, ending no sooner) and the third is refused naming the holder. Before
  /// 2026-11-07 the second was refused `LandingLeaseHeld` naming its own holder, so a renewal only ever succeeded once
  /// the lease had already lapsed, and the entries a landing reached in between were skipped `LeaseEnded`
  /// (`crates/server/tests/landing_fairness.rs` under two spinners a logical CPU).
  #[test]
  fn a_holder_renews_its_own_live_lease_and_no_other_holder_can() {
    crate::daemon::audit_on_shard(|state| {
      let deadline = far(state);
      let first = take_target_lease(state, target(3), 20, LONG_TERM_NS, deadline).unwrap();
      let renewed = take_target_lease(state, target(3), 20, LONG_TERM_NS, deadline)
        .expect("the holder renews its own lease while it holds");
      assert_eq!(renewed.holder, 20);
      assert!(renewed.generation > first.generation);
      assert!(renewed.expires_ns >= first.expires_ns);
      assert_eq!(
        take_target_lease(state, target(3), 21, LONG_TERM_NS, deadline),
        Err(Refusal::LandingLeaseHeld { holder: 20 })
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

  /// Shape: the term a take is waited in the busy-control test: a tenth of a second, short beside the control shard's
  /// busy poll.
  const WAIT_TERM_NS: u64 = 100_000_000;
  /// Shape: the control shard's one busy poll on the wall: three terms, so a take waited one term on the wall gives up.
  const BUSY_NS: u64 = 3 * WAIT_TERM_NS;
  /// Shape: how long the test thread waits for the take's answer: ten busy polls.
  const ANSWER_WAIT_NS: u64 = 10 * BUSY_NS;

  /// R3's class A (A-432). Do: on a two-shard daemon, hold the second shard, standing for the control shard, in one poll
  /// for three terms of wall time, on its CPU a fifth of it (a busy or starved shard); from the first shard take a
  /// target's lease there with a term's window and the take's own deadline far, so only the wait decides; then take the
  /// target for another attempt. Expect: the first take answered with its lease, after more than a term of waiting, and
  /// the second refused `LandingLeaseHeld` by the first. The take waited one term on the wall, so it was compensated and
  /// refused `Overloaded` with the control shard alive and working.
  #[test]
  fn a_lease_take_waits_for_a_control_shard_that_works_past_its_term() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |state| {
      let origin = state.shard;
      let control = state
        .shards
        .iter()
        .copied()
        .find(|&shard| shard != origin)
        .unwrap();
      let deadline = far(state);
      // The busy poll goes first on the control shard's channel, so the take waits behind it.
      let busy = crate::xshard::call_on(origin, control, || {
        crate::daemon::hold_busy(BUSY_NS);
        Some(())
      })
      .unwrap();
      drop(busy);
      let take = slates_rt::futures::spawn(async move {
        let started = std::time::Instant::now();
        let ask = super::LeaseAsk {
          shard: origin,
          control,
          holder: 1,
          term: WAIT_TERM_NS,
          deadline,
        };
        let taken = super::acquire_target_lease(ask, target(3)).await;
        let waited = started.elapsed();
        let second = crate::xshard::call_in_shard_time(
          origin,
          control,
          move |s| take_target_lease(s, target(3), 2, WAIT_TERM_NS, deadline),
          WAIT_TERM_NS,
        )
        .await;
        let _ = sent.send((
          taken.map(|lease| lease.holder),
          waited,
          second.map(|again| again.map(|lease| lease.holder)),
        ));
      })
      .unwrap();
      slates_rt::futures::detach(take).unwrap();
    });
    let answer = answered.recv_timeout(std::time::Duration::from_nanos(ANSWER_WAIT_NS));
    daemon.stop();
    let (taken, waited, second) = answer.expect("the take answered");
    println!(
      "a take behind a busy poll of {BUSY_NS} ns: {taken:?} after {waited:?}; another attempt's: {second:?}"
    );
    assert_eq!(
      taken,
      Ok(1),
      "the take on a control shard alive and working was not answered (after {waited:?})"
    );
    assert!(
      waited > std::time::Duration::from_nanos(WAIT_TERM_NS),
      "the take ended within one term ({waited:?}), before the control shard's busy poll did"
    );
    assert_eq!(
      second,
      Some(Err(Refusal::LandingLeaseHeld { holder: 1 })),
      "the first take holds the target"
    );
  }

  /// §4.15 step 4, ban 8 (AUD-29-03): the control shard holds no more lease records than the leases live at
  /// its last take — a lease whose release never came is released by the next take once its term ends. Do:
  /// lease five targets for a short term, releasing none; pause past the term; lease a sixth. Expect: the
  /// status report counts five, then one.
  #[test]
  fn a_take_releases_the_leases_whose_terms_have_ended() {
    takes_release_the_leases_whose_terms_have_ended(false);
  }

  /// The same history on a machine slow enough that the short leases' takes outlast their term (GAPS, "a lease test
  /// assumes five takes finish inside a 20 ms term": CI run 40077995650 counted 4 where 5 were due). Do: stall past
  /// the term before the last short take. Expect: the history still holds, since the count before the pause is judged
  /// by the time the takes measured, and the final take still leaves its own lease alone.
  #[test]
  fn a_take_releases_the_leases_whose_terms_have_ended_when_the_takes_outlast_the_term() {
    takes_release_the_leases_whose_terms_have_ended(true);
  }

  /// Takes `TARGETS` short leases (stalling past the term before the last when `stalled`), counts them, pauses past
  /// the term, and takes one long lease, which must release every short one.
  ///
  /// The count before the pause is judged by the takes' own clock, not the test's speed: a take releases each lease
  /// whose term ended at or before its reading, and a short take's reading is its lease's expiry less the term. So
  /// the leases held after the last short take are exactly those expiring after its reading: all of them on a machine
  /// that took them inside a term, fewer on one that did not (a stalled history must show fewer, or it stalled
  /// nothing).
  // A history's helper: a refused take is a failed test, as in the `#[test]` bodies.
  #[allow(clippy::unwrap_used)]
  fn takes_release_the_leases_whose_terms_have_ended(stalled: bool) {
    crate::daemon::audit_on_shard(move |state| {
      let deadline = far(state);
      let mut taken = Vec::new();
      for inode in 0..TARGETS {
        if stalled && inode + 1 == TARGETS {
          pause_past_a_short_term();
        }
        taken
          .push(take_target_lease(state, target(inode), inode, SHORT_TERM_NS, deadline).unwrap());
      }
      let last_reading = taken
        .last()
        .map_or(0, |lease| lease.expires_ns.saturating_sub(SHORT_TERM_NS));
      let held = taken
        .iter()
        .filter(|lease| lease.expires_ns > last_reading)
        .count();
      let held = u64::try_from(held).unwrap();
      if stalled {
        assert!(held < TARGETS, "the stall outlasted no term: {held} held");
      }
      assert_eq!(crate::verbs::shard_report(state).target_leases, held);
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

#[cfg(test)]
mod landed_reply_tests {
  use slates_ipc::protocol::{
    HostAnswer, LandingDegradation, LandingDurability, LandingOutcome, ReplyBody, framed_len,
  };
  use slates_ipc::protocol::{Refusal, TargetRefusal};
  use slates_vfs::host::{HostError, OsCode};

  /// A-539 (the numbering's kind carried end to end). Do: pass one number as a host's Win32 error code and as an errno
  /// to the reply's answer, a target's refusal, a crashed landing's cell and a base directory's refusal. Expect the Win32
  /// code in the wire's Win32 variants and the errno in its errno variants, the named kinds by their codes: a reader
  /// never takes one numbering for the other.
  #[test]
  fn a_win32_code_reaches_the_wire_as_one_and_an_errno_as_an_errno() {
    /// Shape: one number in both numberings, `ERROR_ACCESS_DENIED` and `EIO`.
    const BOTH: u32 = 5;
    /// Shape: see `BOTH`.
    const BOTH_ERRNO: i32 = 5;
    let win32 = OsCode::Win32(BOTH);
    let errno = OsCode::Errno(BOTH_ERRNO);
    assert_eq!(
      super::answer_of(HostError::Unavailable(win32)),
      HostAnswer::Win32 { code: BOTH }
    );
    assert_eq!(
      super::answer_of(HostError::NoSpace(errno)),
      HostAnswer::Errno { errno: BOTH_ERRNO }
    );
    assert_eq!(
      crate::error::target_of_host(&HostError::Exists(win32)),
      TargetRefusal::HostWin32 { code: BOTH }
    );
    assert_eq!(
      crate::error::target_of_host(&HostError::Unavailable(errno)),
      TargetRefusal::Host { errno: BOTH_ERRNO }
    );
    // A path on this daemon's own mount carries the platform's deadlock-avoided code in its own numbering (A-227):
    // Windows' `ERROR_POSSIBLE_DEADLOCK` rode in an errno's place before A-539.
    assert_eq!(
      super::answer_of(HostError::OwnMount(win32)),
      HostAnswer::Win32 { code: BOTH }
    );
    assert_eq!(
      crate::error::target_of_host(&HostError::OwnMount(win32)),
      TargetRefusal::HostWin32 { code: BOTH }
    );
    assert_eq!(
      crate::error::target_of_host(&HostError::OwnMount(errno)),
      TargetRefusal::Host { errno: BOTH_ERRNO }
    );
    assert_eq!(
      super::degradation_of(&slates_land::engine::Degradation::Crashed { code: win32 }),
      LandingDegradation::CrashedWin32 { code: BOTH }
    );
    assert_eq!(
      super::degradation_of(&slates_land::engine::Degradation::Crashed { code: errno }),
      LandingDegradation::Crashed { errno: BOTH_ERRNO }
    );
    let base = |code| crate::error::base_unavailable("/base".to_owned(), code);
    assert_eq!(
      base(Some(win32)),
      Refusal::BaseUnavailableWin32 {
        path: "/base".to_owned(),
        code: BOTH
      }
    );
    assert_eq!(
      base(None),
      Refusal::BaseUnavailable {
        path: "/base".to_owned(),
        errno: 0
      }
    );
  }

  /// Shape: the degraded cells the test's landing met: one a directory, several replies' worth at these paths.
  const CELLS: usize = 400;

  /// A-267 (§4.15 step 9). Do: build a finished landing's reply that met 400 directory syncs the host refused (one a
  /// directory, as a filesystem that refuses a directory's sync answers every one), each directory's path about eighty
  /// bytes. Expect: the reply fits one reply chunk, carrying the first cells in the order met and counting the rest,
  /// the two making 400. Before, every cell rode in the reply, and one this long was refused "reply too large for the
  /// bulk area" after the landing had run.
  #[test]
  fn a_landing_that_met_more_degraded_cells_than_one_reply_holds_still_answers() {
    let degraded: Vec<LandingDegradation> = (0..CELLS)
      .map(|n| LandingDegradation::Unsynced {
        dir: format!("/a-directory-whose-sync-the-host-refused-as-a-network-filesystem-may-{n:05}"),
        answer: HostAnswer::Errno { errno: 22 },
      })
      .collect();
    let outcome = LandingOutcome {
      landing: 7,
      state: "done".to_owned(),
      written: 1,
      skipped: 0,
      conflicts: 0,
      failed: 0,
      bytes_written: 1,
      held: 1,
      durability: LandingDurability {
        data_synced: true,
        dirs_synced: false,
        media: false,
        media_requested: false,
        dirs: 0,
      },
      degraded: Vec::new(),
      barriers: 1,
      stage: slates_ipc::protocol::LandingStage::Done,
      verification: slates_ipc::protocol::Verification::Unsigned,
      degraded_elided: 0,
    };
    let reply = super::landed_reply(outcome, degraded.clone());
    let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap();
    assert!(framed_len(&reply) <= chunk, "the reply fits one chunk");
    let ReplyBody::Landed { outcome } = reply else {
      panic!("a landing's reply");
    };
    assert!(outcome.degraded_elided > 0, "the test outgrows one reply");
    assert_eq!(
      outcome.degraded.len() + outcome.degraded_elided as usize,
      CELLS
    );
    assert_eq!(
      outcome.degraded,
      degraded[..outcome.degraded.len()],
      "the first cells, in the order met"
    );
  }

  /// A presentation's reply at `target`: its statement names the target (A-321), with no conflict, the summary and
  /// verdict at their smallest, and the conflicts' count at its widest.
  fn presentation(target: &str) -> ReplyBody {
    ReplyBody::GrantRequired {
      landing: u64::MAX,
      manifest: [u8::MAX; 32],
      summary: slates_ipc::protocol::LandingSummary {
        by_action: Vec::new(),
        bytes: u64::MAX,
        filtered_out: u64::MAX,
        actions: slates_ipc::protocol::ActionTally::default(),
      },
      conflicts: Vec::new(),
      statement: crate::signed_work::landing_statement(&[u8::MAX; 16], target, &[u8::MAX; 32]),
      verification: slates_ipc::protocol::Verification::Unsigned,
      conflicts_elided: u32::MAX,
    }
  }

  /// A-267 with A-321's statement (§4.15 step 2, §4.3). Do: size a presentation's reply whose statement names a target
  /// as long as a grant's listing takes, then one as long as the guard names. Expect the first past one reply chunk,
  /// the guard naming a longest shorter than its target, and the second within the chunk with the guard silent: a
  /// presentation is answered whole or refused before anything is recorded. Since the statement joined the reply
  /// (main's signed landings, 2026-11-18), a target the listing takes could make a reply no chunk holds, refused "reply
  /// too large" after the presentation was recorded.
  #[test]
  fn a_presentation_too_long_for_one_reply_names_the_longest_that_fits() {
    let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap();
    let listed = "t".repeat(super::longest_grantable_target());
    let longest = super::presentation_overflow(&presentation(&listed), listed.len())
      .expect("a grant's longest target overflows a presentation's reply");
    let longest = usize::try_from(longest).unwrap();
    assert!(longest < listed.len(), "{longest} of {}", listed.len());
    let fitting = "t".repeat(longest);
    assert!(
      super::presentation_overflow(&presentation(&fitting), fitting.len()).is_none(),
      "the longest named fits"
    );
    assert!(framed_len(&presentation(&fitting)) <= chunk);
  }
}

#[cfg(test)]
mod owed_audit_tests {
  use std::time::{Duration, Instant};

  use slates_db::catalog::{AuditKind as DbAuditKind, AuditRecord as DbAuditRecord, Principal};
  use slates_vfs::clock::Clock;

  use crate::daemon::LIVENESS_BUDGET_NS;

  /// Shape: how long the test lets its daemon run before the record is owed: past the reap loop's first look, a window
  /// after its start, by a tenth of a window, and short of its second.
  const PAST_THE_FIRST_LOOK_NS: u64 = LIVENESS_BUDGET_NS + LIVENESS_BUDGET_NS / 10;
  /// Shape: how long past a window after the send the test waits for the batch to be sent again: half a window.
  const RESENT_WITHIN_NS: u64 = LIVENESS_BUDGET_NS / 2;
  /// Shape: the pause between the test's looks at the shard's count, short beside half a window.
  const LOOK_PAUSE_NS: u64 = 1_000_000;
  /// Shape: the partition the record is owed from: the second shard's, so the control partition is another shard's.
  const OWING: usize = 1;

  /// A-554 (the owed audit off the tick; A-265's resend). Do: on a two-shard daemon past the reap loop's first look,
  /// with the second shard's acknowledgements withheld so its batch stays owed as if each were lost, owe the control
  /// partition one audit record there and send it as a verb's transaction does; watch the shard's resends. Expect: the
  /// batch sent again no sooner than a window after its send, when its acknowledgement could still be on its way, and
  /// within half a window more, by the shard's deadline keeper. The reap loop's tick looked once a window and sent
  /// again a front it had seen at its look before, so a record owed just after a look was sent again nearly two windows
  /// on.
  #[test]
  fn an_unacknowledged_audit_batch_is_sent_again_a_window_after_its_send() {
    let daemon = crate::daemon::audit_daemon_of(2);
    // The harness waits out the reap loop's first look; the daemon under test runs on.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_nanos(PAST_THE_FIRST_LOOK_NS));
    let sent = Instant::now();
    crate::daemon::observe_at(&daemon, OWING, |state| {
      state.landing.acknowledgement_withheld = true;
      let now = state.clock.monotonic_ns();
      let record = DbAuditRecord {
        seq: 0,
        at_ns: now,
        kind: DbAuditKind::LandingPlanned,
        principal: Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        grant: None,
        landing: Some(1),
        manifest: None,
        outcome: None,
        signature: None,
      };
      super::write_audit(state, vec![record], now).unwrap();
      super::send_owed_audit(state);
    });
    let resent = loop {
      let count = crate::daemon::observe_at(&daemon, OWING, |state| {
        state
          .refusals
          .get(super::AUDIT_RESENT)
          .copied()
          .unwrap_or(0)
      });
      if count > 0 {
        break Some(sent.elapsed());
      }
      if sent.elapsed() > Duration::from_nanos(LIVENESS_BUDGET_NS + RESENT_WITHIN_NS) {
        break None;
      }
      // The harness paces its looks; the daemon under test waits on its wheel.
      #[allow(clippy::disallowed_methods)]
      std::thread::sleep(Duration::from_nanos(LOOK_PAUSE_NS));
    };
    daemon.stop();
    let resent =
      resent.expect("the batch was sent again within half a window past a window after its send");
    println!("an unacknowledged batch was sent again {resent:?} after its send");
    assert!(
      resent >= Duration::from_nanos(LIVENESS_BUDGET_NS),
      "not sent again before its acknowledgement could come: {resent:?}"
    );
  }
}
