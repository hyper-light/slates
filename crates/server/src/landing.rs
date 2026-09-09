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
//! write path runs on Linux and macOS; its tests write into a RAM-backed directory and so are
//! gated to the CI Linux lane (`/dev/shm`), skipping loudly elsewhere, exactly as the Phase 1
//! landing tests are.

use slates_db::Op;
#[cfg(unix)]
use slates_db::catalog::LandingRecord;
use slates_db::catalog::{
  AuditKind as DbAuditKind, AuditRecord as DbAuditRecord, GrantRecord as DbGrantRecord,
  GrantScope as DbGrantScope, GrantState as DbGrantState, GrantSurface,
  LandingState as DbLandingState, Principal, SnapshotId as DbSnapshotId, VolumeId as DbVolumeId,
};
#[cfg(unix)]
use slates_ipc::protocol::{ActionCount, LandingOutcome, LandingSummary};
use slates_ipc::protocol::{
  AuditEntry, GrantScope, GrantSummary, Refusal, ReplyBody, SnapshotId, VolumeId,
};
use slates_land::engine::Audit;
#[cfg(unix)]
use slates_land::engine::{AuditKind, AuditRecord};
#[cfg(unix)]
use slates_land::engine::{LandingRefusal, LandingReport, LandingRequest, Observer, land};
#[cfg(unix)]
use slates_vfs::host::LandFs;
#[cfg(unix)]
use slates_wire::observe::Chokepoint;
#[cfg(unix)]
use slates_land::grant::{GrantId, GrantRefusal};
use slates_land::grant::{GrantScope as LandScope, Grants, Leases, Surface};
#[cfg(unix)]
use slates_land::manifest::{Filter, LandingEntry, Manifest};
#[cfg(unix)]
use slates_land::os::{OsLand, TargetRefusal};
use slates_vfs::clock::Clock;

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

/// The runtime landing state a shard holds (§4.15): the grants, the leases and the audit log,
/// each mirrored into the durable database as it changes.
pub struct LandingState {
  /// The runtime grants (mirrored to `GrantRecord`).
  pub grants: Grants,
  /// The target leases (mirrored to `LandingLeaseRecord`).
  pub leases: Leases,
  /// The runtime audit log (mirrored to `AuditRecord`).
  pub audit: Audit,
  /// The next landing id.
  pub next_landing: u64,
  /// A landing awaiting a grant: its id, the manifest it planned, the volume and the target,
  /// so a grant on the control channel binds the right manifest (§4.15 step 3).
  pub awaiting: std::collections::BTreeMap<u64, Awaiting>,
}

/// A landing presented and waiting for a grant.
#[derive(Clone, Debug)]
pub struct Awaiting {
  /// The manifest hash the grant must bind.
  pub manifest: [u8; 32],
  /// The volume.
  pub volume: DbVolumeId,
  /// The snapshot.
  pub snapshot: DbSnapshotId,
  /// The target.
  pub target: String,
  /// The principal it was presented to.
  pub principal: Principal,
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
      leases: Leases::default(),
      audit: Audit::new(AUDIT_RETAIN),
      next_landing: 1,
      awaiting: std::collections::BTreeMap::new(),
    }
  }
}

/// The identity of a landing: its id and what it lands where.
#[cfg(unix)]
struct LandingIds<'a> {
  landing_id: u64,
  volume: DbVolumeId,
  snapshot: DbSnapshotId,
  target: &'a str,
}

/// Lands `volume`'s snapshot into `target` (§4.15). Without a grant covering the planned
/// manifest, the reply is `GrantRequired`; with one, the landing runs and the reply is
/// `Landed`. Conflicts, a held lease and a mismatched grant are typed refusals.
pub fn land_verb(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
  filter: &slates_ipc::protocol::Filter,
  grant: Option<u64>,
) -> ReplyBody {
  #[cfg(unix)]
  {
    land_verb_unix(state, principal, volume, snapshot, target, filter, grant)
  }
  #[cfg(not(unix))]
  {
    // The write path is the `os` module (Unix), the landing engine's only real writer; the
    // Windows writer arrives with the Windows bridge (Phase 4). The verb refuses cleanly here.
    let _ = (state, principal, volume, snapshot, target, filter, grant);
    refused(Refusal::Unsupported {
      feature: "landing".to_owned(),
    })
  }
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
}

#[cfg(unix)]
impl<H: LandFs> Observer<H> for SpanObserver {
  fn before_write(&mut self, _host: &mut H, _entry: &LandingEntry) {}

  fn after_entry(&mut self, start_ns: u64, end_ns: u64) {
    if self.entries.len() == self.capacity {
      // At the bound: shed the oldest timing (or this one, when the bound is zero) and count the loss.
      if self.entries.pop_front().is_none() {
        self.dropped = self.dropped.saturating_add(1);
        return;
      }
      self.dropped = self.dropped.saturating_add(1);
    }
    self.entries.push_back((start_ns, end_ns));
  }
}

/// Drains a landing's observed entry timings into the shard's telemetry sink as `land.entry` spans
/// (§4.14), each stamped with the request the shard is serving and one of the shard's own span ids, and
/// folds the observer's shed count into the sink's loss total. Called after `land` returns, so the sink
/// is clear of the landing's borrows.
#[cfg(unix)]
fn drain_land_spans(state: &mut ShardState, observer: SpanObserver) {
  let request = state.current_request;
  let dropped = observer.dropped;
  for (start_ns, end_ns) in observer.entries {
    crate::verbs::emit_span(state, Chokepoint::LandEntry, 0, request, start_ns, end_ns);
  }
  state.telemetry.record_dropped(dropped);
}

/// The Unix landing: open the target through `OsLand` and run the engine.
#[cfg(unix)]
fn land_verb_unix(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  target: &str,
  filter: &slates_ipc::protocol::Filter,
  grant: Option<u64>,
) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("land");
  }
  let db_volume = to_db_volume(volume);
  let db_snapshot = snapshot.map_or(record.head, to_db_snapshot);
  // Open the target: this is the only place the server touches a host path for writing, and
  // only under the grant checked below (R1, R10). A path that cannot be opened, or that
  // escapes containment, is a typed refusal with no write.
  let (mut os, land_target) = match OsLand::open_target(std::path::Path::new(target)) {
    Ok(pair) => pair,
    Err(refusal) => return refused(target_refusal(&refusal)),
  };
  let now = state.clock.monotonic_ns();
  let landing_id = state.landing.next_landing;
  let request = LandingRequest {
    landing_id,
    holder: session_of(principal),
    grant: grant.map(GrantId),
    filter: to_land_filter(filter),
    now_ns: now,
    lease_term_ns: state.config.failover_slo_ns,
    media_durability: false,
    large_class_bytes: state.config.large_class_bytes,
    cores: 1,
    max_depth: 1,
    variance_permille: 0,
    costs: None,
    target_entries: None,
  };
  let slot = match state.volumes.get_mut(handle) {
    Ok(s) => s,
    Err(_) => return refused(Refusal::NotFound),
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
    &mut state.landing.leases,
    &mut state.landing.audit,
    &request,
    &mut spans,
  );
  drain_land_spans(state, spans);
  let ids = LandingIds {
    landing_id,
    volume: db_volume,
    snapshot: db_snapshot,
    target,
  };
  match outcome {
    Ok(report) => finish(state, principal, &ids, &report, grant),
    Err(LandingRefusal::GrantRequired(presented)) => {
      present(state, principal, &ids, &presented.manifest)
    }
    Err(LandingRefusal::Conflict(entries)) => refused(Refusal::LandingConflict {
      entries: entries.iter().map(|e| e.path.to_string()).collect(),
    }),
    Err(LandingRefusal::LeaseHeld(held)) => refused(Refusal::LandingLeaseHeld {
      holder: held.holder,
    }),
    Err(LandingRefusal::Grant(GrantRefusal::GrantMismatch { .. })) => {
      refused(Refusal::GrantMismatch)
    }
    Err(LandingRefusal::Grant(_)) => refused(Refusal::GrantInvalid),
    Err(LandingRefusal::Target(e)) => refused(Refusal::TargetUnavailable {
      reason: format!("{e:?}"),
    }),
    Err(LandingRefusal::Volume(e)) => refused(refusal_of_vfs(&e)),
  }
}

/// Records the planned landing (AwaitingGrant) and its audit, and replies `GrantRequired`.
#[cfg(unix)]
fn present(
  state: &mut ShardState,
  principal: &Principal,
  ids: &LandingIds<'_>,
  manifest: &Manifest,
) -> ReplyBody {
  state.landing.next_landing = state.landing.next_landing.saturating_add(1);
  let hash = manifest.hash;
  let landing_id = ids.landing_id;
  state.landing.awaiting.insert(
    landing_id,
    Awaiting {
      manifest: hash,
      volume: ids.volume,
      snapshot: ids.snapshot,
      target: ids.target.to_owned(),
      principal: principal.clone(),
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
  state.landing.next_landing = state.landing.next_landing.saturating_add(1);
  state.landing.awaiting.remove(&landing_id);
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
  let mut ops = vec![Op::LandingRecorded { record }];
  // Mirror the engine's audit records for this landing into the durable log.
  for audit in state.landing.audit.records() {
    if audit.landing == landing_id {
      ops.push(Op::AuditAppended {
        record: db_audit(audit, principal),
      });
    }
  }
  if let Some(id) = grant {
    ops.push(Op::GrantStateChanged {
      id,
      state: DbGrantState::Consumed,
    });
  }
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return refused(crate::error::refusal_of_db(&e));
    }
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
    },
  }
}

/// Issues a grant for a presented landing, binding its manifest (§4.15 step 3). Called from the
/// control channel only (never the ring), so a grant a human did not make cannot exist. The
/// grant is issued into the runtime grants and persisted as a `GrantRecord`; the landing's
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
  let id = state
    .landing
    .grants
    .issue(Surface::Cli, awaiting.manifest, land_scope, now, term_ns);
  let record = DbGrantRecord {
    id: id.0,
    principal: principal.clone(),
    surface: GrantSurface::Cli,
    volume: awaiting.volume,
    snapshot: awaiting.snapshot,
    target: awaiting.target.clone(),
    manifest: awaiting.manifest,
    scope: db_grant_scope(scope, session_of(principal)),
    issued_ns: now,
    expires_ns: now.saturating_add(term_ns),
    state: DbGrantState::Issued,
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
