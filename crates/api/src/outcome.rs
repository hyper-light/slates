//! What a merge, a rebase and a landing come to, typed (§4.12 "The consumer contract", A-299; §4.15, §4.16, D-27). A
//! consumer matches each outcome and acts on it; none is a string to parse.
//!
//! - A landing ([`LandOutcome`]) is written, needs an approval, met conflicts, or was refused by a policy.
//! - A merge ([`MergeOutcome`]) is merged at a green version, needs an approval, met conflicts, or was refused.
//! - A rebase ([`RebaseOutcome`]) moved the work onto the green's head, or met conflicts.
//!
//! **Conflicts are verdicts, never inferences (D-27, D-26).** The merge engine's verdict is accept, identical or
//! conflict, and a landing's per-entry verdict is apply, skip, accept-identical or conflict (§4.15 step 4). A conflict
//! names its path and its class, and nothing is written for it; resolution is the consumer's (§4.16 "Rebase, the only
//! corrective path"; §4.15 `read_base`, rewrite, `rewitness`), after which it merges or lands again.
//!
//! **An approval binds what it shows.** [`LandingApproval::manifest`] is the hash of exactly what an approval writes,
//! and [`MergeApproval::identity`] the increment's identity; the approve calls write or merge only that, and answer
//! the new request when the volume, the work or the disk changed since ([`crate::Instance::approve_landing`],
//! [`crate::Instance::approve_merge`]).

use slates_ipc::protocol::{
  ConflictClass, LandingDegradation, LandingDurability, LandingOutcome, LandingSummary,
  PlannedContent, VolumeId,
};
pub use slates_merge::verdict::MergeConflictClass;

use crate::error::{Error, ProtocolFault};
use crate::instance::LandingRequest;
use crate::policy::PolicyRefusal;
use crate::signed::Verification;

/// What a landing came to, under the caller's write mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LandOutcome {
  /// The landing ran: what reached the disk.
  Written(LandingReport),
  /// The caller's mode asks: what would be written, for the approver. Nothing was written.
  ApprovalRequired(LandingApproval),
  /// The disk changed entries the volume changed too: each conflict, typed. Nothing was written.
  Conflicted(Vec<LandingConflict>),
  /// A policy refused the landing. Nothing was written.
  Refused(PolicyRefusal),
}

/// What a merge came to, under the caller's write mode (§4.16).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
  /// The increment was accepted: the green's new head version, and how the merge's signature was judged.
  Merged {
    /// The green version the increment committed as.
    version: u64,
    /// The merge's signature, as judged.
    verification: Verification,
  },
  /// The caller's mode asks: the merge awaits the approver. Nothing was merged.
  ApprovalRequired(MergeApproval),
  /// The increment met intervening changes: each conflict window, typed. Nothing was merged; rebase, resolve and merge
  /// again.
  Conflicted(MergeConflicts),
  /// A policy refused the merge: its mode forbids it, or the volume requires signed merges. Nothing was merged.
  Refused(PolicyRefusal),
}

/// What a rebase came to (§4.16 "Rebase, the only corrective path"): the green is unchanged either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseOutcome {
  /// Every pending operation mapped cleanly: the head version the work is now based on.
  Rebased {
    /// The work's new base version.
    version: u64,
  },
  /// Pending operations meet intervening changes: the windows to resolve before rebasing again.
  Conflicted(MergeConflicts),
}

/// A work volume as the merge verbs name it: the work and the green it merges into (§4.16).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Work {
  /// The work volume.
  pub volume: VolumeId,
  /// Its green.
  pub green: VolumeId,
}

/// A merge's conflicts: every window, and the green's head version when they were found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflicts {
  /// Each conflicting window, in the order the verdict met them.
  pub conflicts: Vec<MergeConflict>,
  /// The green's head version read right after the verdict: what a rebase moves the work onto, unless another merge
  /// lands first.
  pub head: u64,
}

/// One merge conflict window (§4.16): the file, the range in base coordinates that met an intervening change, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflict {
  /// The file, from the volume's root.
  pub path: String,
  /// The window's offset, in base coordinates.
  pub at: u64,
  /// The window's length (zero for a whole-path namespace conflict, anchored at `at`).
  pub len: u64,
  /// Why the two changes cannot both be accepted.
  pub class: MergeConflictClass,
}

/// One landing conflict (§4.15 step 4): the entry, why it cannot land, and the bytes the volume would land there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingConflict {
  /// The entry, from the target's root.
  pub path: String,
  /// Why it cannot land (the verdict table's class).
  pub class: ConflictClass,
  /// The volume's version of a file it creates or replaces there: its bytes' BLAKE3 and length.
  pub planned: Option<PlannedContent>,
}

/// A landing awaiting an approval (§4.15 step 2): what the approver is shown, and what an approval binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingApproval {
  /// The landing, as it was asked.
  pub request: LandingRequest,
  /// The presentation's id (a presentation lives as long as the client that made it, A-46).
  pub presentation: u64,
  /// The manifest hash an approval binds: exactly what will be written ([`LandingApproval::manifest_text`] as people
  /// read it).
  pub manifest: [u8; 32],
  /// What would be written.
  pub summary: Summary,
  /// How the landing's signature was judged.
  pub verification: Verification,
}

impl LandingApproval {
  /// The manifest hash as text: 64 hexadecimal characters, as `slates land` prints it.
  pub fn manifest_text(&self) -> String {
    self
      .manifest
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect()
  }
}

/// A merge awaiting an approval (§4.16, A-285): the work, and the increment's identity an approval binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeApproval {
  /// The work and its green.
  pub work: Work,
  /// The evidence the merge carried, carried again by its approval (§4.16).
  pub evidence: Vec<[u8; 32]>,
  /// The increment's identity an approval binds: exactly what will be merged.
  pub identity: [u8; 32],
  /// The paths the increment changes.
  pub paths: u64,
  /// How the merge's signature was judged.
  pub verification: Verification,
}

/// What a landing would write (§4.15 step 2): entries per action, bytes, and what the filter left out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Summary {
  /// Entries per action.
  pub actions: ActionTally,
  /// The bytes the landing writes.
  pub bytes: u64,
  /// Entries the filter left out.
  pub filtered_out: u64,
}

/// Entries per landing action (§4.15 "Plan"; the manifest's actions, as [`crate::PlannedAction`] names each).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ActionTally {
  /// Files the disk did not have, written.
  pub create: u64,
  /// Files the disk had, replaced.
  pub replace: u64,
  /// Files the disk had, removed.
  pub delete: u64,
  /// Directories moved whole from their origins.
  pub rename: u64,
  /// Directories made.
  pub mkdir: u64,
  /// Directories removed with everything beneath them.
  pub rmdir: u64,
  /// Symlinks made or replaced.
  pub symlink: u64,
  /// Directories emptied and kept.
  pub clear: u64,
}

impl ActionTally {
  /// Every entry the landing writes.
  pub fn total(&self) -> u64 {
    [
      self.create,
      self.replace,
      self.delete,
      self.rename,
      self.mkdir,
      self.rmdir,
      self.symlink,
      self.clear,
    ]
    .iter()
    .fold(0u64, |sum, count| sum.saturating_add(*count))
  }
}

/// A landing that ran (§4.15 steps 4–9): what reached the disk, its durability and every degraded cell it met.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingReport {
  /// The landing's id.
  pub landing: u64,
  /// How it finished.
  pub state: LandingState,
  /// Entries written.
  pub written: u64,
  /// Entries skipped, or accepted without a write (the disk already held them).
  pub skipped: u64,
  /// Entries that lost their compare-and-swap to an outsider mid-landing and were undone.
  pub conflicts: u64,
  /// Entries the host refused.
  pub failed: u64,
  /// Bytes written to the disk.
  pub bytes_written: u64,
  /// Entries on the disk that stay in the volume's overlay because they did not reach the durability boundary; a resume
  /// advances them.
  pub held: u64,
  /// The durability the landing achieved (§4.15 step 8).
  pub durability: LandingDurability,
  /// Every degraded cell met, in order, as many as one reply holds (A-267).
  pub degraded: Vec<LandingDegradation>,
  /// Degraded cells past what one reply holds (A-267).
  pub degraded_elided: u32,
  /// The batch barriers taken (A-130).
  pub barriers: u64,
  /// The landing's signature, as judged.
  pub verification: Verification,
}

/// How a landing that ran finished (§4.15 "State machines").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LandingState {
  /// Every entry landed.
  Done,
  /// Some entries lost their compare-and-swap and were undone; the rest landed.
  Partial,
  /// Validation refused it; nothing was written.
  Refused,
  /// Cancelled or crashed: each written entry is old or new, never torn, and a resume continues it.
  Aborted,
}

impl LandingState {
  /// The state the daemon names in a landing's outcome (`done`, `partial`, `refused`, `aborted`).
  fn of_name(name: &str) -> Option<LandingState> {
    match name {
      "done" => Some(Self::Done),
      "partial" => Some(Self::Partial),
      "refused" => Some(Self::Refused),
      "aborted" => Some(Self::Aborted),
      _ => None,
    }
  }
}

/// The daemon's landing outcome as the contract types it.
pub(crate) fn report_of(
  outcome: LandingOutcome,
  verification: Verification,
) -> Result<LandingReport, Error> {
  let state =
    LandingState::of_name(&outcome.state).ok_or(Error::Protocol(ProtocolFault::UnknownName {
      field: "landing state",
    }))?;
  Ok(LandingReport {
    landing: outcome.landing,
    state,
    written: outcome.written,
    skipped: outcome.skipped,
    conflicts: outcome.conflicts,
    failed: outcome.failed,
    bytes_written: outcome.bytes_written,
    held: outcome.held,
    durability: outcome.durability,
    degraded: outcome.degraded,
    degraded_elided: outcome.degraded_elided,
    barriers: outcome.barriers,
    verification,
  })
}

/// The daemon's landing summary as the contract types it: each action the manifest names counted under its own field.
pub(crate) fn summary_of(summary: &LandingSummary) -> Result<Summary, Error> {
  let mut actions = ActionTally::default();
  for counted in &summary.by_action {
    let field = match counted.action.as_str() {
      "create" => &mut actions.create,
      "replace" => &mut actions.replace,
      "delete" => &mut actions.delete,
      "rename" => &mut actions.rename,
      "mkdir" => &mut actions.mkdir,
      "rmdir" => &mut actions.rmdir,
      "symlink" => &mut actions.symlink,
      "clear" => &mut actions.clear,
      _ => {
        return Err(Error::Protocol(ProtocolFault::UnknownName {
          field: "landing action",
        }));
      }
    };
    *field = field.saturating_add(counted.count);
  }
  Ok(Summary {
    actions,
    bytes: summary.bytes,
    filtered_out: summary.filtered_out,
  })
}

/// A merge window's class from its wire discriminant (`MergeWindow::class`, `MergeConflictClass as u8`), read against
/// the merge engine's own enum so the two cannot drift apart.
pub(crate) fn merge_class_of(discriminant: u8) -> Result<MergeConflictClass, Error> {
  /// Every class the merge engine has, in its declaration order.
  const CLASSES: [MergeConflictClass; 9] = [
    MergeConflictClass::Overlap,
    MergeConflictClass::AnchoredInDelete,
    MergeConflictClass::SamePositionDiffering,
    MergeConflictClass::RenameRename,
    MergeConflictClass::CreateCreate,
    MergeConflictClass::DeleteModify,
    MergeConflictClass::ModifyDelete,
    MergeConflictClass::TypeChanged,
    MergeConflictClass::MetaMeta,
  ];
  CLASSES
    .iter()
    .copied()
    .find(|class| *class as u8 == discriminant)
    .ok_or(Error::Protocol(ProtocolFault::UnknownName {
      field: "merge conflict class",
    }))
}

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::{ActionCount, LandingSummary};

  use super::{MergeConflictClass, merge_class_of, summary_of};
  use crate::error::{Error, ProtocolFault};

  /// A-299: do type a summary naming every action the manifest has, and one naming an action it has not; expect each
  /// count under its own field and the total their sum, and the unknown name refused as a protocol fault, never counted
  /// under another.
  #[test]
  fn a_summary_counts_each_action_under_its_own_name() {
    let names = [
      "create", "replace", "delete", "rename", "mkdir", "rmdir", "symlink", "clear",
    ];
    let summary = LandingSummary {
      by_action: names
        .iter()
        .zip(1u64..)
        .map(|(name, count)| ActionCount {
          action: (*name).to_owned(),
          count,
        })
        .collect(),
      bytes: 7,
      filtered_out: 2,
    };
    let typed = summary_of(&summary).unwrap();
    let actions = typed.actions;
    assert_eq!(
      [
        actions.create,
        actions.replace,
        actions.delete,
        actions.rename,
        actions.mkdir,
        actions.rmdir,
        actions.symlink,
        actions.clear
      ],
      [1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!(actions.total(), 36);
    assert_eq!((typed.bytes, typed.filtered_out), (7, 2));
    let unknown = LandingSummary {
      by_action: vec![ActionCount {
        action: "chmod".to_owned(),
        count: 1,
      }],
      ..LandingSummary::default()
    };
    assert_eq!(
      summary_of(&unknown),
      Err(Error::Protocol(ProtocolFault::UnknownName {
        field: "landing action"
      }))
    );
  }

  /// A-299, D-27: do read every merge window class back from the discriminant the daemon sends (`class as u8`), and one
  /// past the last; expect each class itself, and the stranger refused as a protocol fault.
  #[test]
  fn every_merge_class_reads_back_from_its_discriminant() {
    for class in [
      MergeConflictClass::Overlap,
      MergeConflictClass::AnchoredInDelete,
      MergeConflictClass::SamePositionDiffering,
      MergeConflictClass::RenameRename,
      MergeConflictClass::CreateCreate,
      MergeConflictClass::DeleteModify,
      MergeConflictClass::ModifyDelete,
      MergeConflictClass::TypeChanged,
      MergeConflictClass::MetaMeta,
    ] {
      assert_eq!(merge_class_of(class as u8), Ok(class));
    }
    assert!(matches!(
      merge_class_of(MergeConflictClass::MetaMeta as u8 + 1),
      Err(Error::Protocol(ProtocolFault::UnknownName { .. }))
    ));
  }
}
