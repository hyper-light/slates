//! Observing the daemon's grants and its audit log, typed (§4.12 "The consumer contract", A-299; §4.13 "Grants",
//! §4.14, §4.15 "Audit"). The wire carries a grant's state, an audit record's kind and a landing's stage typed
//! (`GrantSummary::grant_state`, `AuditEntry::audit_kind` and `stage`, appended 2026-11-18); this module reads those,
//! never the words beside them, which are for people. Until then it read the words, and a word it did not know was a
//! protocol fault. A record of a landing as a whole carries how the landing's signature was judged (A-321), typed as
//! the wire carries it.

use slates_ipc::protocol::{self as wire, AuditEntry, GrantScope, GrantSummary, VolumeId};

use crate::signed::Verification;

/// A grant as the daemon lists it (§4.13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRecord {
  /// The grant's id.
  pub id: u64,
  /// The volume it lands.
  pub volume: VolumeId,
  /// The target it lands into.
  pub target: String,
  /// The manifest hash it binds.
  pub manifest: [u8; 32],
  /// One landing, or the session's later landings of the volume into the target.
  pub scope: GrantScope,
  /// Where it stands.
  pub state: GrantState,
}

/// Where a grant stands (§4.15 "State machines").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GrantState {
  /// Issued and not yet used up.
  Issued,
  /// A single-use grant's landing finished.
  Consumed,
  /// Its term passed, or its session ended.
  Expired,
  /// Revoked.
  Revoked,
}

/// One record of the audit log (§4.15, §4.14): append-only, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
  /// The record's sequence.
  pub seq: u64,
  /// When, in the daemon's monotonic nanoseconds.
  pub at_ns: u64,
  /// What happened.
  pub kind: AuditKind,
  /// The grant it concerns, when bound.
  pub grant: Option<u64>,
  /// The landing it concerns, when bound.
  pub landing: Option<u64>,
  /// The manifest it concerns, when bound.
  pub manifest: Option<[u8; 32]>,
  /// The landing's state when the record was made, for a finished landing: [`LandingStage::Refused`] or
  /// [`LandingStage::Aborted`] for a granted landing that was refused, after the records of the steps it took.
  pub outcome: Option<LandingStage>,
  /// How the landing's signature was judged (signed work, A-321), on a record of the landing as a whole — its
  /// presentation's and a granted run's [`AuditKind::LandingPlanned`], its [`AuditKind::LandingFinished`]
  /// ([`Verification::Unsigned`] when it carried none): the landing's provenance. `None` on every other record, and on
  /// the end of a landing refused before anything named its manifest, which had no statement to judge.
  pub verification: Option<Verification>,
}

/// What an audit record records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AuditKind {
  /// A grant was issued (by the launcher's approval, or by a bypass policy).
  GrantIssued,
  /// A grant was revoked.
  GrantRevoked,
  /// A landing was presented.
  LandingPlanned,
  /// A landing's entries were validated against the disk.
  LandingValidated,
  /// An entry was written.
  EntryWritten,
  /// An entry was refused.
  EntryRefused,
  /// A landing finished.
  LandingFinished,
}

/// A landing's stage (§4.15 "State machines"): the four terminal states a finished landing reports
/// ([`crate::LandingState`]) and the stages before them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LandingStage {
  /// Its manifest is being planned.
  Planning,
  /// Presented, awaiting an approval.
  AwaitingGrant,
  /// Its entries are being validated against the disk.
  Validating,
  /// Its entries are being written.
  Writing,
  /// Its written entries are being made durable.
  Syncing,
  /// The volume is advancing past what landed.
  Advancing,
  /// Every entry landed.
  Done,
  /// Some entries were undone; the rest landed.
  Partial,
  /// Validation refused it; nothing was written.
  Refused,
  /// Cancelled or crashed; a resume continues it.
  Aborted,
}

/// The daemon's grant listing, typed.
pub(crate) fn grant_of(grant: GrantSummary) -> GrantRecord {
  GrantRecord {
    id: grant.id,
    volume: grant.volume,
    target: grant.target,
    manifest: grant.manifest,
    scope: grant.scope,
    state: match grant.grant_state {
      wire::GrantState::Issued => GrantState::Issued,
      wire::GrantState::Consumed => GrantState::Consumed,
      wire::GrantState::Expired => GrantState::Expired,
      wire::GrantState::Revoked => GrantState::Revoked,
    },
  }
}

/// One audit record, typed.
pub(crate) fn audit_of(entry: AuditEntry) -> AuditRecord {
  AuditRecord {
    seq: entry.seq,
    at_ns: entry.at_ns,
    kind: match entry.audit_kind {
      wire::AuditKind::GrantIssued => AuditKind::GrantIssued,
      wire::AuditKind::GrantRevoked => AuditKind::GrantRevoked,
      wire::AuditKind::LandingPlanned => AuditKind::LandingPlanned,
      wire::AuditKind::LandingValidated => AuditKind::LandingValidated,
      wire::AuditKind::EntryWritten => AuditKind::EntryWritten,
      wire::AuditKind::EntryRefused => AuditKind::EntryRefused,
      wire::AuditKind::LandingFinished => AuditKind::LandingFinished,
    },
    grant: entry.grant,
    landing: entry.landing,
    manifest: entry.manifest,
    outcome: entry.stage.map(stage_of),
    verification: entry.verification.map(Verification::of_wire),
  }
}

/// A landing's stage, typed.
fn stage_of(stage: wire::LandingStage) -> LandingStage {
  match stage {
    wire::LandingStage::Planning => LandingStage::Planning,
    wire::LandingStage::AwaitingGrant => LandingStage::AwaitingGrant,
    wire::LandingStage::Validating => LandingStage::Validating,
    wire::LandingStage::Writing => LandingStage::Writing,
    wire::LandingStage::Syncing => LandingStage::Syncing,
    wire::LandingStage::Advancing => LandingStage::Advancing,
    wire::LandingStage::Done => LandingStage::Done,
    wire::LandingStage::Partial => LandingStage::Partial,
    wire::LandingStage::Refused => LandingStage::Refused,
    wire::LandingStage::Aborted => LandingStage::Aborted,
  }
}

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::{self as wire, AuditEntry, GrantScope, GrantSummary, VolumeId};

  use super::{AuditKind, GrantState, LandingStage, audit_of, grant_of};

  /// A-299: do type a grant listing of each typed state, its word for people set to a stranger; expect each state from
  /// the typed field, the word never read (2026-11-18: until then a stranger word was a protocol fault).
  #[test]
  fn each_grant_state_is_read_from_its_typed_field() {
    let grant = |state: wire::GrantState| GrantSummary {
      id: 1,
      volume: VolumeId { bytes: [1; 16] },
      target: "/t".to_owned(),
      manifest: [2; 32],
      scope: GrantScope::Once,
      state: "a word for people".to_owned(),
      grant_state: state,
    };
    for (typed, state) in [
      (wire::GrantState::Issued, GrantState::Issued),
      (wire::GrantState::Consumed, GrantState::Consumed),
      (wire::GrantState::Expired, GrantState::Expired),
      (wire::GrantState::Revoked, GrantState::Revoked),
    ] {
      assert_eq!(grant_of(grant(typed)).state, state);
    }
  }

  /// A-299: do type audit records of each typed kind with and without a stage, their words for people set to
  /// strangers; expect each kind and stage from the typed fields.
  #[test]
  fn each_audit_kind_and_stage_is_read_from_its_typed_field() {
    let entry = |kind: wire::AuditKind, stage: Option<wire::LandingStage>| AuditEntry {
      seq: 3,
      at_ns: 4,
      kind: "a word for people".to_owned(),
      grant: None,
      landing: Some(5),
      manifest: None,
      outcome: stage.map(|_| "a word for people".to_owned()),
      audit_kind: kind,
      stage,
      verification: None,
    };
    for (typed, kind) in [
      (wire::AuditKind::GrantIssued, AuditKind::GrantIssued),
      (wire::AuditKind::GrantRevoked, AuditKind::GrantRevoked),
      (wire::AuditKind::LandingPlanned, AuditKind::LandingPlanned),
      (
        wire::AuditKind::LandingValidated,
        AuditKind::LandingValidated,
      ),
      (wire::AuditKind::EntryWritten, AuditKind::EntryWritten),
      (wire::AuditKind::EntryRefused, AuditKind::EntryRefused),
      (wire::AuditKind::LandingFinished, AuditKind::LandingFinished),
    ] {
      assert_eq!(audit_of(entry(typed, None)).kind, kind);
    }
    for (typed, stage) in [
      (wire::LandingStage::Planning, LandingStage::Planning),
      (
        wire::LandingStage::AwaitingGrant,
        LandingStage::AwaitingGrant,
      ),
      (wire::LandingStage::Validating, LandingStage::Validating),
      (wire::LandingStage::Writing, LandingStage::Writing),
      (wire::LandingStage::Syncing, LandingStage::Syncing),
      (wire::LandingStage::Advancing, LandingStage::Advancing),
      (wire::LandingStage::Done, LandingStage::Done),
      (wire::LandingStage::Partial, LandingStage::Partial),
      (wire::LandingStage::Refused, LandingStage::Refused),
      (wire::LandingStage::Aborted, LandingStage::Aborted),
    ] {
      assert_eq!(
        audit_of(entry(wire::AuditKind::LandingFinished, Some(typed))).outcome,
        Some(stage)
      );
    }
  }
}
