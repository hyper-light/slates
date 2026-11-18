//! A grant's claim and its proof (§4.13 "Grants", R10), kept apart from what makes the proof, so the library's grant
//! call takes any proof the daemon accepts.
//!
//! **The claim** is exactly what a grant authorizes: one presented landing, by its id, the manifest hash a person
//! approved, the scope and the term. **The proof** is the human authority's word over that claim. Today the daemon
//! accepts one form, the keyed hash under the anchor's issuer secret that an [`crate::Issuer`] makes. Ada chose the
//! proof's next form on 2026-11-18: a signature by a key only the human holds — an ssh-agent key with confirmation on
//! each use, or a FIDO touch, with a prompt on the anchor's terminal where there is neither, on every platform; an
//! embedding host's interface may present a WebAuthn assertion as one form of it. Those arrive as further
//! [`GrantProof`] variants and [`GrantAuthority`] implementations; [`crate::Instance::grant`] and
//! [`crate::Instance::land_approved`] do not change.
//!
//! A host whose person proves out of band (a browser's assertion) holds the presented plan on its [`crate::Instance`],
//! asks for the proof over the plan's [`GrantClaim`], and calls `grant` with what comes back; one with a synchronous
//! authority calls `land_approved`, which presents again, compares, proves and lands in one step.

use slates_ipc::protocol::GrantScope;

use crate::error::Error;

/// What a grant authorizes (§4.13): exactly one presented landing, the manifest a person approved, the scope and the
/// term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GrantClaim {
  /// The presented landing's id ([`crate::LandingPlan::landing`]).
  pub landing: u64,
  /// The manifest hash the person approved ([`crate::LandingPlan::manifest`]).
  pub manifest: [u8; 32],
  /// One landing of exactly this manifest, or the session's later landings of the volume into the target.
  pub scope: GrantScope,
  /// How long the grant stands, in nanoseconds ([`crate::GRANT_TERM_NS`] by default).
  pub term_ns: u64,
}

/// The human authority's proof over a [`GrantClaim`], in a form the daemon verifies before it issues the grant. Further
/// forms are added (see the module doc), so match it with a wildcard arm.
#[derive(Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

/// What can prove a claim when asked, in this process: the issuer secret's holder ([`crate::Issuer`]) today; a
/// human-held key's agent once the daemon accepts its signatures.
pub trait GrantAuthority {
  /// The proof of `claim`. Refused typed when this authority cannot prove now (no handoff, no published secret, the
  /// person declined).
  fn prove(&self, claim: &GrantClaim) -> Result<GrantProof, Error>;
}
