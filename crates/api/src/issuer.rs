//! The issuer authority (§4.13 "Grants", R10; A-299): what lets the launcher approve a landing an agent's write mode
//! asks about, enroll or revoke a consumer, and confirm a consensus recovery.
//!
//! The daemon mints an issuer secret at every start and publishes it into the anchor's segment; a proof is a keyed hash
//! under it of exactly what is granted (the landing, its manifest, the scope and the term), which the daemon verifies
//! before it accepts the grant, and a secret minted by a previous daemon proves nothing to this one. So the authority is
//! holding the segment: the anchor's own tree holds it (on Linux the segment is a descriptor only the anchor's children
//! inherit), and on macOS and Windows a process the anchor's user gave its name to. Under Ada's rule of 2026-11-18
//! (A-299) that holder is the launcher — the host that started the instance and passes each agent its write mode — and
//! never an agent: an agent holds only its consumer's capability, and the daemon refuses a grant kind on the ring, the
//! SDKs' and the MCP server's channels whatever they claim.
//!
//! An [`Issuer`] keeps the handoff, never the secret: each proof attaches the segment, reads the secret the running
//! daemon published, proves, and lets the mapping go, so a daemon restart between two grants is met with its own
//! secret and the secret is held in this process only for the length of one proof.
//!
//! Moved here from the CLI on 2026-11-18 (A-278): `slates grant`, `enroll`, `revoke`, `run` and `recover` use it, as an
//! embedding host's launcher does.

use slates_anchor::AnchorSegment;
use slates_anchor::layout::ISSUER_SECRET_BYTES;
use slates_anchor::segment::{ENV_HANDOFF, ENV_LEN};
use slates_ipc::delivery::Capability;
use slates_machine::facts::{Facts, Identity};
use slates_mem::Handoff;

use crate::error::{Denied, Error, IssuerFault};
use crate::grant::{GrantAuthority, GrantClaim, GrantProof};
use crate::instance::Instance;
use crate::launcher::LauncherSecret;

/// Derived: a grant's default validity, in nanoseconds — one hour, the span §4.15's session scope names as the longest
/// a human's single approval should stand without renewal (an agent that lands later than that re-presents its
/// manifest for a fresh look). `slates grant --term SECONDS` overrides it.
pub const GRANT_TERM_NS: u64 = 3_600 * 1_000_000_000;

/// The authority over one instance: the handoff of its anchor's segment, or the launcher's own secret (A-299).
#[derive(Clone, Debug)]
pub struct Issuer {
  /// Where the proofs' key comes from.
  authority: Authority,
}

/// Where an [`Issuer`]'s key comes from.
#[derive(Clone, Debug)]
enum Authority {
  /// The anchor's segment, attached for one proof at a time: the issuer secret the running daemon published.
  Segment {
    /// The segment's handoff: a descriptor this process holds (Linux), or the object's name (macOS, Windows).
    handoff: Handoff,
    /// The segment's length.
    len: usize,
    /// This machine's identity, which the segment's header must name.
    identity: Identity,
  },
  /// The launcher's own secret, which it provided at the instance's start and keeps.
  Launcher(LauncherSecret),
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
    let handoff = value(ENV_HANDOFF).ok_or_else(|| no_authority(IssuerFault::NoHandoff))?;
    let len = value(ENV_LEN)
      .ok_or_else(|| {
        no_authority(IssuerFault::Malformed {
          reason: "no segment length (SLATES_ANCHOR_LEN)",
        })
      })?
      .parse::<usize>()
      .map_err(|_| {
        no_authority(IssuerFault::Malformed {
          reason: "the segment length (SLATES_ANCHOR_LEN) is not a number",
        })
      })?;
    let handoff = match handoff.parse::<i32>() {
      Ok(descriptor) if cfg!(target_os = "linux") => Handoff::Descriptor(descriptor),
      _ => Handoff::Name(handoff.to_owned()),
    };
    Ok(Issuer {
      authority: Authority::Segment {
        handoff,
        len,
        identity: Facts::query().identity,
      },
    })
  }

  /// The launcher's authority from the secret it provided at the instance's start ([`crate::StartSpec::with_launcher`],
  /// A-299): no segment is attached, so it proves after the launcher's own restart as before it, on every platform. The
  /// daemon verifies launcher-class proofs under it beside the issuer secret (A-285's owner's piece); a consensus
  /// recovery still needs the segment ([`IssuerFault::LauncherCannotRecover`]).
  pub fn from_launcher_secret(secret: &LauncherSecret) -> Issuer {
    Issuer {
      authority: Authority::Launcher(secret.clone()),
    }
  }

  /// The issuer secret the running daemon published: the segment attached, read, and let go. Refused
  /// [`IssuerFault::NotPublished`] while the daemon has published none (an all-zero secret is no authority).
  fn secret(&self) -> Result<[u8; ISSUER_SECRET_BYTES], Error> {
    let (handoff, len, identity) = match &self.authority {
      Authority::Launcher(secret) => return Ok(*secret.bytes()),
      Authority::Segment {
        handoff,
        len,
        identity,
      } => (handoff, *len, identity),
    };
    let segment = AnchorSegment::attach(handoff, len, identity)
      .map_err(|e| no_authority(IssuerFault::Attach(e)))?;
    let secret = segment
      .issuer_secret()
      .map_err(|e| no_authority(IssuerFault::Attach(e)))?;
    if secret.iter().all(|byte| *byte == 0) {
      return Err(no_authority(IssuerFault::NotPublished));
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

  /// The proof a `SetPolicy` request carries (A-285, A-299): the keyed hash of exactly `subject` and `policy` under the
  /// secret the running daemon published, with the policy domain's own tag, so it proves nothing else.
  pub(crate) fn policy_proof(
    &self,
    subject: &slates_ipc::protocol::PolicySubject,
    policy: &slates_ipc::protocol::WritePolicy,
    version: u64,
  ) -> Result<[u8; 32], Error> {
    Ok(slates_server::landing::policy_proof(
      &self.secret()?,
      subject,
      policy,
      version,
    ))
  }

  /// The proof that confirms the consensus recovery plan `plan` (§4.8, A-19), for an instance whose operator holds no
  /// recovery key of their own.
  pub fn recovery_proof(&self, plan: &[u8; 32]) -> Result<[u8; 32], Error> {
    if let Authority::Launcher(_) = self.authority {
      return Err(no_authority(IssuerFault::LauncherCannotRecover));
    }
    Ok(slates_server::recovery_proof(&self.secret()?, plan))
  }
}

/// This process holds no usable issuer authority: why, as the contract's denial.
fn no_authority(fault: IssuerFault) -> Error {
  Error::Denied(Denied::NoAuthority(fault))
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

  /// The keyed hash of exactly `work` and its increment `identity`, under the merge approval's own domain (A-285).
  fn prove_merge(
    &self,
    work: slates_ipc::protocol::VolumeId,
    identity: &[u8; 32],
  ) -> Result<GrantProof, Error> {
    let secret = self.secret()?;
    Ok(GrantProof::IssuerHash(
      slates_server::landing::merge_approval_proof(&secret, work, identity),
    ))
  }
}
