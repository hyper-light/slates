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
//! embedding host's launcher does; `slates credential` enrolls and revokes version-control credentials with it (A-625
//! part 3).

use slates_anchor::AnchorSegment;
use slates_anchor::layout::ISSUER_SECRET_BYTES;
use slates_anchor::segment::{ENV_HANDOFF, ENV_LEN};
use slates_ipc::delivery::Capability;
use slates_ipc::protocol::{CredentialMaterial, Principal};
use slates_machine::facts::{Facts, Identity};
use slates_mem::Handoff;

use crate::error::{Denied, Error, IssuerFault, Unsupported};
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
  /// The anchor's issuer route on Linux (A-754): its abstract name and the secret the anchor printed, presented for the
  /// segment's descriptor at each proof.
  #[cfg(target_os = "linux")]
  Route {
    /// The route's abstract name.
    name: String,
    /// The secret the anchor printed for it.
    secret: [u8; slates_ipc::issuer_route::SECRET_BYTES],
    /// This machine's identity, which the segment's header must name.
    identity: Identity,
  },
}

impl Issuer {
  /// The authority in this process's environment: the anchor's handoff (`SLATES_ANCHOR`, `SLATES_ANCHOR_LEN`), which the
  /// anchor's children inherit and, on macOS and Windows, the anchor's user exports from what the anchor printed.
  /// Refused [`IssuerFault::NoHandoff`] when there is none — the process was not started from the anchor's session.
  pub fn from_environment() -> Result<Issuer, Error> {
    #[cfg(target_os = "linux")]
    if std::env::var_os(ENV_HANDOFF).is_none()
      && let Some(issuer) = Issuer::from_route_environment()?
    {
      return Ok(issuer);
    }
    let pairs: Vec<(String, String)> = [ENV_HANDOFF, ENV_LEN]
      .into_iter()
      .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
      .collect();
    Issuer::from_handoff(&pairs)
  }

  /// The anchor's issuer route the environment names on Linux (A-754: `SLATES_ANCHOR_ISSUER_ROUTE` and
  /// `SLATES_ANCHOR_ISSUER_SECRET`, as the anchor printed them for its user to export); none when neither is set, and
  /// refused [`IssuerFault::Malformed`] for one without the other or a secret not in the printed form.
  #[cfg(target_os = "linux")]
  fn from_route_environment() -> Result<Option<Issuer>, Error> {
    use slates_ipc::issuer_route::{ENV_ISSUER_ROUTE, ENV_ISSUER_SECRET, parse_secret};
    match (
      std::env::var(ENV_ISSUER_ROUTE).ok(),
      std::env::var(ENV_ISSUER_SECRET).ok(),
    ) {
      (None, None) => Ok(None),
      (Some(name), Some(secret)) => {
        let secret = parse_secret(secret.trim()).ok_or_else(|| {
          no_authority(IssuerFault::Malformed {
            reason: "the issuer secret (SLATES_ANCHOR_ISSUER_SECRET) is not the anchor's printed form",
          })
        })?;
        Ok(Some(Issuer {
          authority: Authority::Route {
            name,
            secret,
            identity: Facts::query().identity,
          },
        }))
      }
      _ => Err(no_authority(IssuerFault::Malformed {
        reason: "the issuer route and its secret (SLATES_ANCHOR_ISSUER_ROUTE, SLATES_ANCHOR_ISSUER_SECRET) go together",
      })),
    }
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
    let segment = match &self.authority {
      Authority::Launcher(secret) => return Ok(*secret.bytes()),
      Authority::Segment {
        handoff,
        len,
        identity,
      } => AnchorSegment::attach(handoff, *len, identity)
        .map_err(|e| no_authority(IssuerFault::Attach(e)))?,
      #[cfg(target_os = "linux")]
      Authority::Route {
        name,
        secret,
        identity,
      } => {
        use std::os::fd::AsRawFd;
        // The anchor answers within a turn of its loop, which the daemon's observation budget bounds.
        let patience = std::time::Duration::from_nanos(slates_server::watchdog::WINDOW_CEILING_NS);
        let (object, len) = slates_ipc::issuer_route::claim(name, secret, patience)
          .map_err(|e| no_authority(IssuerFault::Route(e)))?;
        let len = usize::try_from(len).map_err(|_| {
          no_authority(IssuerFault::Malformed {
            reason: "the issuer route named a segment longer than this platform maps",
          })
        })?;
        // Attaching duplicates the descriptor, so the claimed one closes when this arm ends.
        AnchorSegment::attach(&Handoff::Descriptor(object.as_raw_fd()), len, identity)
          .map_err(|e| no_authority(IssuerFault::Attach(e)))?
      }
    };
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

  /// Enrolls the SSH key whose blob is `blob` for `principal` until `expires_ns` on the wall clock, if one is given
  /// (§4.13, §4.17.2 "Access"; A-625 part 3); its id, OpenSSH's fingerprint of the blob. The daemon takes a key only
  /// when SSH's user authentication would.
  pub fn enroll_ssh_key(
    &self,
    instance: &mut Instance,
    principal: Principal,
    blob: Vec<u8>,
    expires_ns: Option<u64>,
  ) -> Result<[u8; 32], Error> {
    self.enroll_credential(
      instance,
      principal,
      CredentialMaterial::SshKey { blob },
      expires_ns,
    )
  }

  /// Mints a token for `principal`, good until `expires_ns` on the wall clock if one is given, and enrolls it by its
  /// hash and its CRAM-MD5 keyed contexts (A-625 part 3, A-692): the token's text, shown this once, and its id. The
  /// daemon is sent the hash and the contexts alone, so the token never crosses a channel; whoever holds the text
  /// presents it as Basic's password or as a bearer token over HTTP, and as CRAM-MD5's password over `svn://`, the id
  /// in lowercase hex as its user name.
  pub fn enroll_token(
    &self,
    instance: &mut Instance,
    principal: Principal,
    expires_ns: Option<u64>,
  ) -> Result<(String, [u8; 32]), Error> {
    let mut random = [0u8; slates_server::credentials::TOKEN_RANDOM_BYTES];
    slates_transport::handshake::secure_random(&mut random)
      .map_err(|_| Error::Unsupported(Unsupported::SecureRandom))?;
    let token = slates_server::credentials::token_text(&random);
    let id = slates_server::credentials::token_id(token.as_bytes());
    let cram_md5_contexts = slates_server::credentials::cram_md5_contexts(token.as_bytes());
    self.enroll_credential(
      instance,
      principal,
      CredentialMaterial::HttpToken {
        id,
        cram_md5_contexts,
      },
      expires_ns,
    )?;
    Ok((token, id))
  }

  /// Enrolls `material` for `principal` until `expires_ns`, proven under the running daemon's secret; its id.
  fn enroll_credential(
    &self,
    instance: &mut Instance,
    principal: Principal,
    material: CredentialMaterial,
    expires_ns: Option<u64>,
  ) -> Result<[u8; 32], Error> {
    let secret = self.secret()?;
    let proof =
      slates_server::credentials::enroll_proof(&secret, &principal, &material, expires_ns);
    instance
      .client
      .enroll_credential(principal, material, expires_ns, proof)
      .map_err(|e| instance.error(e))
  }

  /// Revokes the version-control credential named `id` (§4.13; A-625 part 3): a later login or request presenting it
  /// is refused.
  pub fn revoke_credential(&self, instance: &mut Instance, id: [u8; 32]) -> Result<(), Error> {
    let secret = self.secret()?;
    let proof = slates_server::credentials::revoke_proof(&secret, &id);
    instance
      .client
      .revoke_credential(id, proof)
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

  /// The proof a `SetSigners` request carries (A-321): the keyed hash of exactly `volume`, `keys` and the policy
  /// `version` they replace, under the secret the running daemon published, with the signers domain's own tag.
  pub(crate) fn signers_proof(
    &self,
    volume: slates_ipc::protocol::VolumeId,
    keys: &[Vec<u8>],
    version: u64,
  ) -> Result<[u8; 32], Error> {
    Ok(slates_server::landing::signers_proof(
      &self.secret()?,
      volume,
      keys,
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
