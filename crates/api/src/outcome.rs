//! What a merge, a rebase and a landing come to, typed (§4.12 "The consumer contract", A-299; §4.15, §4.16, D-27). A
//! consumer matches each outcome and acts on it; none is a string to parse.
//!
//! - A landing ([`LandOutcome`]) is written, needs an approval, met conflicts, or was refused by a policy.
//! - A merge ([`MergeOutcome`]) is merged at a green version, needs an approval, met conflicts, or was refused.
//! - A rebase ([`RebaseOutcome`]) moved the work onto the green's head, or met conflicts.
//! - A move of a volume to another fleet member ([`MoveProgress`], A-291) is under way, or done.
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
use crate::signed::{Verification, WorkSignature};

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
  /// A policy refused the merge: its mode forbids it, or the work or the green requires signed merges and the merge's
  /// signature was absent or did not verify. Nothing was merged.
  Refused(PolicyRefusal),
}

/// A green version's merge record as a consumer reads it back (§4.16, A-321; [`crate::Instance::merge_record`]): how the
/// signature of the merge that made the version was judged when it merged, its provenance, kept for as long as the green
/// keeps the version's accepted record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeRecord {
  /// The version.
  pub version: u64,
  /// How the merge's signature was judged (`Unsigned` for version 0, the origin, and for a merge that carried none).
  pub verification: Verification,
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
  /// How the landing's signature was judged against this manifest (A-321): `Unsigned` when it carried none.
  pub verification: Verification,
  /// The landing's statement (A-321): the exact bytes a signature over this landing signs, as the daemon states them —
  /// the volume, the target as the request names it and this manifest, one field a line. Sign it under the namespace
  /// `slates-work` (`ssh-keygen -Y sign -n slates-work -f KEY FILE`); the signature verifies for this manifest alone.
  pub statement: Vec<u8>,
  /// The signature the landing carried, carried again by its approval (A-321): [`crate::Instance::approve_landing`]
  /// presents and lands with it, so the landing is recorded with its verdict. An approver may set one made over
  /// [`LandingApproval::statement`] before approving; a signature permits nothing either way.
  pub signature: Option<WorkSignature>,
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
  /// The signature the merge carried, carried again by its approval (A-321): the held merge kept nothing, and the
  /// approved one is recorded with it.
  pub signature: Option<WorkSignature>,
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
  /// Files whose mode or Windows attribute bits alone are set, their bytes kept (A-458).
  pub chmod: u64,
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
      self.chmod,
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
  /// How the landing's signature was judged against the manifest it wrote (A-321), as its finished audit record keeps
  /// it ([`crate::Instance::audit`]): `Unsigned` when it carried none.
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
  /// A finished landing's state from its outcome's typed stage (A-299, 2026-11-18: until then read from its name); a
  /// stage short of the end is no finished landing's.
  fn of_stage(stage: slates_ipc::protocol::LandingStage) -> Option<LandingState> {
    use slates_ipc::protocol::LandingStage as Stage;
    match stage {
      Stage::Done => Some(Self::Done),
      Stage::Partial => Some(Self::Partial),
      Stage::Refused => Some(Self::Refused),
      Stage::Aborted => Some(Self::Aborted),
      Stage::Planning
      | Stage::AwaitingGrant
      | Stage::Validating
      | Stage::Writing
      | Stage::Syncing
      | Stage::Advancing => None,
    }
  }
}

/// The daemon's landing outcome as the contract types it, with how its signature was judged (A-321).
pub(crate) fn report_of(outcome: LandingOutcome) -> Result<LandingReport, Error> {
  let state =
    LandingState::of_stage(outcome.stage).ok_or(Error::Protocol(ProtocolFault::UnknownName {
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
    verification: Verification::of_wire(outcome.verification),
  })
}

/// The daemon's landing summary as the contract types it: each action the manifest names counted under its own field,
/// read from the typed tally (A-299; the names are for people).
pub(crate) fn summary_of(summary: &LandingSummary) -> Summary {
  let typed = summary.actions;
  Summary {
    actions: ActionTally {
      create: typed.create,
      replace: typed.replace,
      delete: typed.delete,
      rename: typed.rename,
      mkdir: typed.mkdir,
      rmdir: typed.rmdir,
      symlink: typed.symlink,
      clear: typed.clear,
      chmod: typed.chmod,
    },
    bytes: summary.bytes,
    filtered_out: summary.filtered_out,
  }
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
  use slates_ipc::protocol::LandingSummary;

  use super::{MergeConflictClass, merge_class_of, summary_of};
  use crate::error::{Error, ProtocolFault};

  /// A-299: do type a summary whose typed tally counts every action the manifest has, its names for people left
  /// empty; expect each count under its own field and the total their sum. The names are never read (2026-11-18: until
  /// then the summary was read from them, and a name the contract did not know was a protocol fault).
  #[test]
  fn a_summary_counts_each_action_under_its_own_field() {
    let summary = LandingSummary {
      by_action: Vec::new(),
      bytes: 7,
      filtered_out: 2,
      actions: slates_ipc::protocol::ActionTally {
        create: 1,
        replace: 2,
        delete: 3,
        rename: 4,
        mkdir: 5,
        rmdir: 6,
        symlink: 7,
        clear: 8,
        chmod: 9,
      },
    };
    let typed = summary_of(&summary);
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
        actions.clear,
        actions.chmod
      ],
      [1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
    assert_eq!(actions.total(), 45);
    assert_eq!((typed.bytes, typed.filtered_out), (7, 2));
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

/// Where a volume's move to another fleet member stands after one ask ([`crate::Instance::move_volume`]; §4.10
/// "Ownership follows the writer", A-291). A move goes on between asks; a consumer asks again until it is
/// [`MoveProgress::Moved`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveProgress {
  /// Under way: the volume is frozen on its owner, which waits for its records to be placed or for the configuration to
  /// name the new owner. Every other verb on it is refused for now ([`crate::RetryCause::Overloaded`]).
  Moving {
    /// The member it moves to.
    to: u64,
  },
  /// Done: the configuration names the new owner, where the volume's verbs go now. The new owner answers them once it
  /// serves the volume (until then [`crate::RetryCause::Overloaded`]), and the old owner lets its copy go once the new
  /// owner has placed the volume under its own placement.
  Moved {
    /// The member it moved to.
    to: u64,
  },
}
