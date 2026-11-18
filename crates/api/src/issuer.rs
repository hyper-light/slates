//! The issuer authority (§4.13 "Grants", R10): what lets a confirmation surface grant a landing, enroll or revoke a
//! consumer, and confirm a consensus recovery.
//!
//! Only an authenticated human confirmation surface holds grant-issuer authority. The daemon mints an issuer secret at
//! every start and publishes it into the anchor's segment; a proof is a keyed hash under it of exactly what is granted
//! (the landing, its manifest, the scope and the term), which the daemon verifies before it accepts the grant, and a
//! secret minted by a previous daemon proves nothing to this one. So the authority is holding the segment: the anchor's
//! own tree holds it (on Linux the segment is a descriptor only the anchor's children inherit), and on macOS and
//! Windows a process the anchor's user gave its name to (the anchor prints the variables to export). The ring, the SDKs
//! and the MCP server never hold it, and the daemon refuses a grant kind on their channels whatever they claim.
//!
//! An [`Issuer`] keeps the handoff, never the secret: each proof attaches the segment, reads the secret the running
//! daemon published, proves, and lets the mapping go, so a daemon restart between two grants is met with its own
//! secret and the secret is held in this process only for the length of one proof.
//!
//! Moved here from the CLI on 2026-11-18 (A-278): `slates grant`, `enroll`, `revoke`, `run` and `recover` use it, as an
//! embedding host's confirmation surface does.

use slates_anchor::AnchorSegment;
use slates_anchor::layout::ISSUER_SECRET_BYTES;
use slates_anchor::segment::{ENV_HANDOFF, ENV_LEN};
use slates_ipc::delivery::Capability;
use slates_machine::facts::{Facts, Identity};
use slates_mem::Handoff;

use crate::error::{Error, IssuerFault};
use crate::grant::{GrantAuthority, GrantClaim, GrantProof};
use crate::instance::Instance;

/// Derived: a grant's default validity, in nanoseconds — one hour, the span §4.15's session scope names as the longest
/// a human's single approval should stand without renewal (an agent that lands later than that re-presents its
/// manifest for a fresh look). `slates grant --term SECONDS` overrides it.
pub const GRANT_TERM_NS: u64 = 3_600 * 1_000_000_000;

/// The issuer authority of one instance: the handoff of its anchor's segment.
#[derive(Clone, Debug)]
pub struct Issuer {
  /// The segment's handoff: a descriptor this process holds (Linux), or the object's name (macOS, Windows).
  handoff: Handoff,
  /// The segment's length.
  len: usize,
  /// This machine's identity, which the segment's header must name.
  identity: Identity,
}

impl Issuer {
  /// The authority in this process's environment: the anchor's handoff (`SLATES_ANCHOR`, `SLATES_ANCHOR_LEN`), which the
  /// anchor's children inherit and, on macOS and Windows, the anchor's user exports from what the anchor printed.
  /// Refused [`IssuerFault::NoHandoff`] when there is none — the process was not started from the anchor's session.
  pub fn from_environment() -> Result<Issuer, Error> {
    let pairs: Vec<(String, String)> = [ENV_HANDOFF, ENV_LEN]
      .into_iter()
      .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
      .collect();
    Issuer::from_handoff(&pairs)
  }

  /// The authority a handoff names, as the environment pairs the anchor hands its children (`SLATES_ANCHOR`,
  /// `SLATES_ANCHOR_LEN`; other pairs are ignored). On Linux the handoff is a descriptor number of this process, so it
  /// names the segment only in a process that holds that descriptor.
  pub fn from_handoff(pairs: &[(String, String)]) -> Result<Issuer, Error> {
    let value = |key: &str| {
      pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
    };
    let handoff = value(ENV_HANDOFF).ok_or(Error::Issuer(IssuerFault::NoHandoff))?;
    let len = value(ENV_LEN)
      .ok_or(Error::Issuer(IssuerFault::Malformed {
        reason: "no segment length (SLATES_ANCHOR_LEN)",
      }))?
      .parse::<usize>()
      .map_err(|_| {
        Error::Issuer(IssuerFault::Malformed {
          reason: "the segment length (SLATES_ANCHOR_LEN) is not a number",
        })
      })?;
    let handoff = match handoff.parse::<i32>() {
      Ok(descriptor) if cfg!(target_os = "linux") => Handoff::Descriptor(descriptor),
      _ => Handoff::Name(handoff.to_owned()),
    };
    Ok(Issuer {
      handoff,
      len,
      identity: Facts::query().identity,
    })
  }

  /// The issuer secret the running daemon published: the segment attached, read, and let go. Refused
  /// [`IssuerFault::NotPublished`] while the daemon has published none (an all-zero secret is no authority).
  fn secret(&self) -> Result<[u8; ISSUER_SECRET_BYTES], Error> {
    let segment = AnchorSegment::attach(&self.handoff, self.len, &self.identity)
      .map_err(|e| Error::Issuer(IssuerFault::Attach(e)))?;
    let secret = segment
      .issuer_secret()
      .map_err(|e| Error::Issuer(IssuerFault::Attach(e)))?;
    if secret.iter().all(|byte| *byte == 0) {
      return Err(Error::Issuer(IssuerFault::NotPublished));
    }
    Ok(secret)
  }

  /// Enrolls a consumer under host account `account` (§4.13 "Principals"); the consumer id and its capability, shown
  /// this once: the caller delivers it to the workload on an inherited descriptor (`slates_ipc::delivery`), never
  /// through a channel other agents share.
  pub fn enroll(&self, instance: &mut Instance, account: u32) -> Result<(u64, Capability), Error> {
    let secret = self.secret()?;
    let proof = slates_server::landing::enroll_proof(&secret, account);
    instance
      .client
      .enroll(account, proof)
      .map_err(|e| instance.error(e))
  }

  /// Revokes a consumer's enrollment (§4.13): acknowledged once every shard has marked the consumer's bound channels,
  /// so every later verb from them is refused `ConsumerRevoked`.
  pub fn revoke(&self, instance: &mut Instance, consumer: u64) -> Result<(), Error> {
    let secret = self.secret()?;
    let proof = slates_server::landing::revoke_proof(&secret, consumer);
    instance
      .client
      .revoke(consumer, proof)
      .map_err(|e| instance.error(e))
  }

  /// The proof that confirms the consensus recovery plan `plan` (§4.8, A-19), for an instance whose operator holds no
  /// recovery key of their own.
  pub fn recovery_proof(&self, plan: &[u8; 32]) -> Result<[u8; 32], Error> {
    Ok(slates_server::recovery_proof(&self.secret()?, plan))
  }
}

impl GrantAuthority for Issuer {
  /// The keyed hash of exactly `claim` — its landing, manifest, scope and term — under the secret the running daemon
  /// published, which that daemon verifies before it issues the grant (§4.13).
  fn prove(&self, claim: &GrantClaim) -> Result<GrantProof, Error> {
    let secret = self.secret()?;
    Ok(GrantProof::IssuerHash(slates_server::landing::grant_proof(
      &secret,
      claim.landing,
      &claim.manifest,
      claim.scope,
      claim.term_ns,
    )))
  }
}
