//! A grant's claim and its proof (§4.13 "Grants", R10; A-299), kept apart from what makes the proof, so the library's
//! grant call takes any proof the daemon accepts.
//!
//! **The claim** is exactly what a grant authorizes: one presented landing, by its id, the manifest hash its approver
//! saw, the scope and the term. **The proof** is the authority's word over that claim. Under Ada's rule of 2026-11-18
//! (A-299) the authority is the launcher's — whoever started the instance, or the harness it hands an agent's write
//! mode — and never an agent's own: an agent holds only its consumer's capability. Today the daemon accepts one form,
//! the keyed hash under the anchor's issuer secret that an [`crate::Issuer`] makes. No person-only key is required;
//! signatures are for signed work ([`crate::signed`]), which never gates permission.
//!
//! [`crate::Instance::approve_landing`] is the whole step for a consumer: it presents the landing again, compares the
//! manifest with the approved one, has a [`GrantAuthority`] prove the claim, grants and lands. A host whose approver
//! answers on another client issues the grant there with [`crate::Instance::grant`] from the presentation's id and
//! manifest ([`crate::LandingApproval`]); the presenting client then lands under it.

use slates_ipc::protocol::{GrantScope, VolumeId};

use crate::error::Error;

/// What a grant authorizes (§4.13): exactly one presented landing, the manifest a person approved, the scope and the
/// term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantClaim {
  /// The presented landing's id ([`crate::LandingApproval::presentation`]).
  pub landing: u64,
  /// The manifest hash the approver saw ([`crate::LandingApproval::manifest`]).
  pub manifest: [u8; 32],
  /// One landing of exactly this manifest, or the session's later landings of the volume into the target.
  pub scope: GrantScope,
  /// How long the grant stands, in nanoseconds ([`crate::GRANT_TERM_NS`] by default).
  pub term_ns: u64,
}

/// The authority's proof over a [`GrantClaim`], in a form the daemon verifies before it issues the grant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GrantProof {
  /// The keyed hash of the claim under the anchor's issuer secret (`slates_server::landing::grant_proof`), which the
  /// daemon that minted the secret verifies.
  IssuerHash([u8; 32]),
}

impl GrantProof {
  /// The proof's bytes as the `Grant` request carries them.
  pub(crate) fn wire(&self) -> [u8; 32] {
    match self {
      Self::IssuerHash(proof) => *proof,
    }
  }
}

impl std::fmt::Debug for GrantProof {
  /// A proof is a credential for its one claim: its bytes are left out of logs.
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::IssuerHash(_) => f.write_str("IssuerHash(..)"),
    }
  }
}

/// What can prove a claim when asked, in this process: the launcher's authority, the issuer secret's holder
/// ([`crate::Issuer`]).
pub trait GrantAuthority {
  /// The proof of `claim`. Refused typed when this authority cannot prove now (no handoff, no published secret).
  fn prove(&self, claim: &GrantClaim) -> Result<GrantProof, Error>;

  /// The proof that approves merging the increment `identity` of `work` (A-285: a merge an ask mode held). Refused
  /// typed as [`GrantAuthority::prove`] is.
  fn prove_merge(&self, work: VolumeId, identity: &[u8; 32]) -> Result<GrantProof, Error>;
}
