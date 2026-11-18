//! Write permission as the consumer contract states it (§4.12 "The consumer contract", A-299; Ada, 2026-11-18): merging
//! a work volume into its green (§4.16) and landing a volume onto a host directory (§4.15) follow the permission the
//! user gave the harness or agent asking to write.
//!
//! - **Bypass:** slates merges and writes at once.
//! - **Ask:** the call returns a typed approval request ([`crate::LandOutcome::ApprovalRequired`],
//!   [`crate::MergeOutcome::ApprovalRequired`]) the harness presents, and the write happens once it is approved.
//! - **Forbid:** a typed refusal ([`crate::LandOutcome::Refused`], [`crate::MergeOutcome::Refused`]).
//!
//! No key, signature or person-only proof decides it. The launcher (kiln passes each agent's mode) or the harness sets
//! a policy per subject — an enrolled consumer, that is an agent's session (§4.13 "Principals"), or a volume — and an
//! act follows the stricter of its caller's and its volume's ([`WritePolicy::meet`]). An agent never raises its own:
//! setting a policy needs the launcher's authority, which the daemon refuses on a consumer's channel, and an agent holds
//! only its consumer's capability. A subject with no policy set keeps what every subject did before A-299
//! ([`WritePolicy::UNSET`]): its merges run at once, since a merge is a change in memory (§4.16), and its landings ask,
//! since a write to disk waits for an approval (R10).
//!
//! The daemon keeps and enforces the policies (A-285, kiln agent g220); this module is the contract's half: the
//! modes, the policy, its subjects and the typed refusal a forbidden act answers.

use std::fmt;

use slates_ipc::protocol::{self as wire, VolumeId};

use crate::error::{Error, Unsupported};
use crate::signed::Verification;

/// What a write mode governs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WriteAct {
  /// A merge: a work volume's increment submitted into its green (§4.16).
  Merge,
  /// A landing: a volume's changes written to a host directory (§4.15).
  Land,
}

/// The permission a harness or agent was given to write, as slates enforces it. Ordered from the mode that lets the
/// least through to the one that lets the most: `Forbid < Ask < Bypass`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WriteMode {
  /// Writing is forbidden: the act is refused, typed, and nothing is written.
  Forbid,
  /// Each act waits for an approval: the call returns what would be written, and the write happens when the policy's
  /// holder approves exactly that.
  Ask,
  /// Full bypass: the act runs at once.
  Bypass,
}

impl WriteMode {
  /// The stricter of two modes: the one that lets less through.
  pub fn stricter(self, other: WriteMode) -> WriteMode {
    self.min(other)
  }

  /// Whether setting `self` where `current` holds would raise it — what an agent's own channel may never do.
  pub fn raises(self, current: WriteMode) -> bool {
    self > current
  }
}

/// A subject's write policy: a mode for each act, and whether merges must be signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WritePolicy {
  /// The mode a merge follows.
  pub merge: WriteMode,
  /// The mode a landing follows.
  pub land: WriteMode,
  /// Merges must carry a signature that verifies (signed work, [`crate::signed`]): on a volume, every merge into it;
  /// on a consumer, every merge it makes. A requirement on what is accepted, never a permission: an unsigned merge into
  /// such a volume is refused even in bypass, and a signed one still follows its mode.
  pub require_signed_merges: bool,
}

impl WritePolicy {
  /// The policy of a subject nobody set one for, which is what every subject had before write modes existed: merges
  /// at once (a merge changes memory, §4.16), landings asked (a write to disk waits for an approval, R10), signatures
  /// not required. Agreed with the daemon's side (A-285), which applies the same when no policy is recorded.
  pub const UNSET: WritePolicy = WritePolicy {
    merge: WriteMode::Bypass,
    land: WriteMode::Ask,
    require_signed_merges: false,
  };

  /// The mode `act` follows.
  pub fn mode(&self, act: WriteAct) -> WriteMode {
    match act {
      WriteAct::Merge => self.merge,
      WriteAct::Land => self.land,
    }
  }

  /// The policy that holds where two subjects' policies meet — the caller's consumer and the volume: each act's
  /// stricter mode, and signed merges required when either requires them.
  pub fn meet(&self, other: &WritePolicy) -> WritePolicy {
    WritePolicy {
      merge: self.merge.stricter(other.merge),
      land: self.land.stricter(other.land),
      require_signed_merges: self.require_signed_merges || other.require_signed_merges,
    }
  }

  /// Whether setting `self` where `current` holds would raise any part of it: a mode let through further, or a
  /// signature requirement dropped.
  pub fn raises(&self, current: &WritePolicy) -> bool {
    self.merge.raises(current.merge)
      || self.land.raises(current.land)
      || (current.require_signed_merges && !self.require_signed_merges)
  }
}

/// Whose policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PolicySubject {
  /// An enrolled consumer: an agent's session (§4.13 "Principals"), by its consumer id.
  Consumer(u64),
  /// A volume.
  Volume(VolumeId),
}

/// Why a policy refused a merge or a landing: nothing was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyRefusal {
  /// The act refused.
  pub act: WriteAct,
  /// The subject whose policy refused it.
  pub by: PolicySubject,
  /// Why.
  pub reason: PolicyReason,
}

/// What in a policy refused an act.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyReason {
  /// The act's mode is [`WriteMode::Forbid`].
  Forbidden,
  /// The volume requires signed merges, and the merge's signature was absent or did not verify: how it was judged.
  SignatureRequired {
    /// The merge's signature, as judged.
    verification: Verification,
  },
}

impl WriteAct {
  /// The act as the wire names it.
  pub(crate) fn of_wire(act: wire::WriteAct) -> WriteAct {
    match act {
      wire::WriteAct::Merge => Self::Merge,
      wire::WriteAct::Land => Self::Land,
    }
  }
}

impl WriteMode {
  /// The mode as the wire carries it.
  fn wire(self) -> wire::WriteMode {
    match self {
      Self::Forbid => wire::WriteMode::Forbid,
      Self::Ask => wire::WriteMode::Ask,
      Self::Bypass => wire::WriteMode::Bypass,
    }
  }

  /// The mode as the wire names it.
  fn of_wire(mode: wire::WriteMode) -> WriteMode {
    match mode {
      wire::WriteMode::Forbid => Self::Forbid,
      wire::WriteMode::Ask => Self::Ask,
      wire::WriteMode::Bypass => Self::Bypass,
    }
  }
}

impl WritePolicy {
  /// The policy as the wire carries it (A-285). The wire keeps no signed-merge requirement yet, so a policy that sets
  /// one is refused [`Unsupported::SignedWork`] here, before anything is sent: a requirement the daemon would drop is
  /// never sent as if it held.
  pub(crate) fn wire(&self) -> Result<wire::WritePolicy, Error> {
    if self.require_signed_merges {
      return Err(Error::Unsupported(Unsupported::SignedWork));
    }
    Ok(wire::WritePolicy {
      merge: self.merge.wire(),
      land: self.land.wire(),
    })
  }

  /// The policy as the wire names it; no signed-merge requirement, which the wire does not keep yet.
  pub(crate) fn of_wire(policy: wire::WritePolicy) -> WritePolicy {
    WritePolicy {
      merge: WriteMode::of_wire(policy.merge),
      land: WriteMode::of_wire(policy.land),
      require_signed_merges: false,
    }
  }
}

impl PolicySubject {
  /// The subject as the wire carries it.
  pub(crate) fn wire(self) -> wire::PolicySubject {
    match self {
      Self::Consumer(consumer) => wire::PolicySubject::Consumer { consumer },
      Self::Volume(volume) => wire::PolicySubject::Volume { volume },
    }
  }

  /// The subject as the wire names it.
  pub(crate) fn of_wire(subject: wire::PolicySubject) -> PolicySubject {
    match subject {
      wire::PolicySubject::Consumer { consumer } => Self::Consumer(consumer),
      wire::PolicySubject::Volume { volume } => Self::Volume(volume),
    }
  }
}

impl PolicyRefusal {
  /// The daemon's `PolicyForbids { act, by }` (A-285): the act's mode forbids it.
  pub(crate) fn forbidden(act: wire::WriteAct, by: wire::PolicySubject) -> PolicyRefusal {
    PolicyRefusal {
      act: WriteAct::of_wire(act),
      by: PolicySubject::of_wire(by),
      reason: PolicyReason::Forbidden,
    }
  }
}

impl fmt::Display for PolicyRefusal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let act = match self.act {
      WriteAct::Merge => "merge",
      WriteAct::Land => "landing",
    };
    let whose = match self.by {
      PolicySubject::Consumer(consumer) => format!("consumer {consumer}'s"),
      PolicySubject::Volume(volume) => format!("volume {}'s", crate::volume_id_text(volume)),
    };
    match self.reason {
      PolicyReason::Forbidden => write!(f, "the {act} is forbidden by {whose} write policy"),
      PolicyReason::SignatureRequired { verification } => write!(
        f,
        "{whose} policy requires signed merges, and this merge's signature is {}",
        match verification {
          Verification::Unsigned => "absent".to_owned(),
          Verification::Invalid { reason } => format!("invalid ({reason})"),
          Verification::Verified { .. } => "verified".to_owned(),
        }
      ),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::{WriteAct, WriteMode, WritePolicy};

  /// A-299, Ada's write modes: do meet two policies, and ask whether one raises another; expect each act to follow the
  /// stricter mode and a signature requirement held by either to hold, and an agent's attempt to let more through — a
  /// mode raised, or a signature requirement dropped — to count as raising, while a lowering does not.
  #[test]
  fn a_policy_meets_another_at_the_stricter_and_a_raise_is_named() {
    let consumer = WritePolicy {
      merge: WriteMode::Bypass,
      land: WriteMode::Ask,
      require_signed_merges: false,
    };
    let volume = WritePolicy {
      merge: WriteMode::Ask,
      land: WriteMode::Forbid,
      require_signed_merges: true,
    };
    let met = consumer.meet(&volume);
    assert_eq!(met.mode(WriteAct::Merge), WriteMode::Ask);
    assert_eq!(met.mode(WriteAct::Land), WriteMode::Forbid);
    assert!(met.require_signed_merges);
    assert_eq!(met, volume.meet(&consumer), "the meet is symmetric");

    assert!(WriteMode::Bypass.raises(WriteMode::Ask));
    assert!(!WriteMode::Forbid.raises(WriteMode::Ask));
    assert!(
      !consumer.raises(&WritePolicy::UNSET),
      "unset already merges at once and asks for landings"
    );
    let landing_bypass = WritePolicy {
      land: WriteMode::Bypass,
      ..WritePolicy::UNSET
    };
    assert!(
      landing_bypass.raises(&WritePolicy::UNSET),
      "a landing bypass is a raise over unset's ask"
    );
    let dropped = WritePolicy {
      require_signed_merges: false,
      ..volume
    };
    assert!(
      dropped.raises(&volume),
      "dropping a signature requirement is a raise"
    );
    let lowered = WritePolicy {
      merge: WriteMode::Forbid,
      ..volume
    };
    assert!(!lowered.raises(&volume), "a lowering is not");
  }
}
