//! Observing the daemon's grants and its audit log, typed (§4.12 "The consumer contract", A-299; §4.13 "Grants",
//! §4.14, §4.15 "Audit"). The wire names a grant's state, an audit record's kind and a landing's state in words from
//! closed tables the daemon keeps (`crates/server/src/landing.rs`: `grant_state_name`, `audit_kind_name`,
//! `landing_state_name`); this module reads each word into its enum in one place, and a word the table does not hold is
//! a protocol fault — a client and a daemon of different builds — never a guess.

use slates_ipc::protocol::{AuditEntry, GrantScope, GrantSummary, VolumeId};

use crate::error::{Error, ProtocolFault};

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
  /// The landing's state when the record was made, for a finished landing.
  pub outcome: Option<LandingStage>,
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
pub(crate) fn grant_of(grant: GrantSummary) -> Result<GrantRecord, Error> {
  let state = match grant.state.as_str() {
    "issued" => GrantState::Issued,
    "consumed" => GrantState::Consumed,
    "expired" => GrantState::Expired,
    "revoked" => GrantState::Revoked,
    _ => return Err(unknown("grant state")),
  };
  Ok(GrantRecord {
    id: grant.id,
    volume: grant.volume,
    target: grant.target,
    manifest: grant.manifest,
    scope: grant.scope,
    state,
  })
}

/// One audit record, typed.
pub(crate) fn audit_of(entry: AuditEntry) -> Result<AuditRecord, Error> {
  let kind = match entry.kind.as_str() {
    "grant_issued" => AuditKind::GrantIssued,
    "grant_revoked" => AuditKind::GrantRevoked,
    "landing_planned" => AuditKind::LandingPlanned,
    "landing_validated" => AuditKind::LandingValidated,
    "entry_written" => AuditKind::EntryWritten,
    "entry_refused" => AuditKind::EntryRefused,
    "landing_finished" => AuditKind::LandingFinished,
    _ => return Err(unknown("audit kind")),
  };
  let outcome = entry.outcome.as_deref().map(stage_of).transpose()?;
  Ok(AuditRecord {
    seq: entry.seq,
    at_ns: entry.at_ns,
    kind,
    grant: entry.grant,
    landing: entry.landing,
    manifest: entry.manifest,
    outcome,
  })
}

/// A landing's stage by the name the daemon writes.
fn stage_of(name: &str) -> Result<LandingStage, Error> {
  Ok(match name {
    "planning" => LandingStage::Planning,
    "awaiting_grant" => LandingStage::AwaitingGrant,
    "validating" => LandingStage::Validating,
    "writing" => LandingStage::Writing,
    "syncing" => LandingStage::Syncing,
    "advancing" => LandingStage::Advancing,
    "done" => LandingStage::Done,
    "partial" => LandingStage::Partial,
    "refused" => LandingStage::Refused,
    "aborted" => LandingStage::Aborted,
    _ => return Err(unknown("landing state")),
  })
}

/// A word the daemon's tables do not hold.
fn unknown(field: &'static str) -> Error {
  Error::Protocol(ProtocolFault::UnknownName { field })
}

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::{AuditEntry, GrantScope, GrantSummary, VolumeId};

  use super::{AuditKind, GrantState, LandingStage, audit_of, grant_of};
  use crate::error::{Error, ProtocolFault};

  /// A-299: do type a grant listing's every state word and one the daemon never writes; expect each state, and the
  /// stranger a protocol fault, never a guess.
  #[test]
  fn each_grant_state_word_is_typed_and_a_stranger_refused() {
    let grant = |state: &str| GrantSummary {
      id: 1,
      volume: VolumeId { bytes: [1; 16] },
      target: "/t".to_owned(),
      manifest: [2; 32],
      scope: GrantScope::Once,
      state: state.to_owned(),
    };
    for (word, state) in [
      ("issued", GrantState::Issued),
      ("consumed", GrantState::Consumed),
      ("expired", GrantState::Expired),
      ("revoked", GrantState::Revoked),
    ] {
      assert_eq!(grant_of(grant(word)).map(|record| record.state), Ok(state));
    }
    assert_eq!(
      grant_of(grant("spent")),
      Err(Error::Protocol(ProtocolFault::UnknownName {
        field: "grant state"
      }))
    );
  }

  /// A-299: do type audit records of each kind with and without an outcome, and one whose outcome names no stage;
  /// expect each kind and stage, and the stranger a protocol fault.
  #[test]
  fn each_audit_word_is_typed_and_a_stranger_refused() {
    let entry = |kind: &str, outcome: Option<&str>| AuditEntry {
      seq: 3,
      at_ns: 4,
      kind: kind.to_owned(),
      grant: None,
      landing: Some(5),
      manifest: None,
      outcome: outcome.map(str::to_owned),
    };
    for (word, kind) in [
      ("grant_issued", AuditKind::GrantIssued),
      ("grant_revoked", AuditKind::GrantRevoked),
      ("landing_planned", AuditKind::LandingPlanned),
      ("landing_validated", AuditKind::LandingValidated),
      ("entry_written", AuditKind::EntryWritten),
      ("entry_refused", AuditKind::EntryRefused),
      ("landing_finished", AuditKind::LandingFinished),
    ] {
      assert_eq!(
        audit_of(entry(word, None)).map(|record| record.kind),
        Ok(kind)
      );
    }
    for (word, stage) in [
      ("planning", LandingStage::Planning),
      ("awaiting_grant", LandingStage::AwaitingGrant),
      ("validating", LandingStage::Validating),
      ("writing", LandingStage::Writing),
      ("syncing", LandingStage::Syncing),
      ("advancing", LandingStage::Advancing),
      ("done", LandingStage::Done),
      ("partial", LandingStage::Partial),
      ("refused", LandingStage::Refused),
      ("aborted", LandingStage::Aborted),
    ] {
      assert_eq!(
        audit_of(entry("landing_finished", Some(word))).map(|record| record.outcome),
        Ok(Some(stage))
      );
    }
    assert!(matches!(
      audit_of(entry("landing_finished", Some("landed"))),
      Err(Error::Protocol(ProtocolFault::UnknownName { .. }))
    ));
    assert!(matches!(
      audit_of(entry("grant_minted", None)),
      Err(Error::Protocol(ProtocolFault::UnknownName { .. }))
    ));
  }
}
