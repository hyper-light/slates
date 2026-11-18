//! An instance as an embedding host holds it (§4.12 "Embedding", A-278): one client of one running daemon, and the
//! verbs a host drives — the volume lifecycle of §4.4, the serving forms of §4.6, drift and landing of §4.15, and the
//! observation of §4.14.
//!
//! **One client, held.** An [`Instance`] is a connected `slates_client::Client`, so a host makes one and keeps it: a
//! landing's presentation lives as long as the client that presented it (A-46), so a plan a person is shown can be
//! granted and landed on the same client, which a command per verb could not do (kiln re-planned before every grant).
//! The client reconnects under its own id when its daemon restarts and resends what it had in flight, which the daemon
//! answers from its completion records (§4.9), so a held instance outlives a daemon restart.
//!
//! **Landing under a grant (§4.15, §4.13).** [`Instance::plan_landing`] presents a landing and returns its plan: the
//! manifest hash a person approves, the summary of what it writes, and the conflicts found before any write. A grant
//! binds exactly that manifest, its target and its volume, and the daemon issues one only under a proof of the human's
//! authority over its claim ([`crate::grant`]): [`Instance::grant`] takes any proof form the daemon accepts, made by
//! this process or out of band. [`Instance::land_approved`] is a synchronous confirmation surface's whole step: present
//! the landing again, and prove, grant and land it only if its manifest is still the one the person approved, else
//! return the new plan for them to see — so a grant never covers more than what a person said yes to, however long
//! they took.

use slates_client::{Attachment, Client, CreateSpec, Landing, ShardsStream, VhostUserDevice};
use slates_ipc::protocol::{
  AuditEntry, DaemonReport, Filter, GrantScope, GrantSummary, Intent, LandingOutcome,
  LandingSummary, PlannedEntry, ReplyBody, RequestBody, SnapshotId, StatusReport, TelemetryReport,
  VolumeId, VolumeSummary,
};

use crate::error::Error;
use crate::grant::{GrantAuthority, GrantClaim, GrantProof};

/// A client of one running instance, and every verb this crate offers.
#[derive(Debug)]
pub struct Instance {
  /// The instance's name (its rendezvous, §4.7).
  pub(crate) name: String,
  /// The connected client.
  pub(crate) client: Client,
}

/// A landing a person is asked to approve (§4.15 step 2): its id, the manifest hash a grant must bind, what it would
/// write, and the conflicts found before any write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingPlan {
  /// The landing's id on this client (a presentation lives as long as its client, A-46).
  pub landing: u64,
  /// The manifest hash a grant binds: what the person approves.
  pub manifest: [u8; 32],
  /// What the landing would write: entries per action, bytes, entries the filter left out.
  pub summary: LandingSummary,
  /// Conflicts found before any write: entries the disk changed under the volume (empty when the landing is clean).
  pub conflicts: Vec<String>,
}

impl LandingPlan {
  /// The grant claim that approves exactly this plan, for `scope` and `term_ns` ([`crate::GRANT_TERM_NS`] by default):
  /// what a confirmation surface has proved ([`GrantAuthority`]) and passes to [`Instance::grant`].
  pub fn claim(&self, scope: GrantScope, term_ns: u64) -> GrantClaim {
    GrantClaim {
      landing: self.landing,
      manifest: self.manifest,
      scope,
      term_ns,
    }
  }

  /// The manifest hash as text: 64 hexadecimal characters, as `slates land` prints it and `slates grant` takes it.
  pub fn manifest_text(&self) -> String {
    self
      .manifest
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect()
  }
}

/// What presenting a landing came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Planned {
  /// A plan that needs a grant before anything is written.
  Plan(LandingPlan),
  /// Nothing needed a grant: the landing ran as it was (nothing to write, or a session grant covered it).
  Landed(LandingOutcome),
}

/// What landing exactly what a person approved came to ([`Instance::land_approved`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Approved {
  /// The landing ran under a grant bound to the approved manifest, or needed none.
  Landed(LandingOutcome),
  /// The landing's manifest is no longer the approved one (the volume or the disk changed since): nothing was
  /// granted or written, and this is the new plan to show.
  Changed(LandingPlan),
}

/// A `land` reply as this crate types it.
fn planned(landing: Landing) -> Planned {
  match landing {
    Landing::Landed(outcome) => Planned::Landed(outcome),
    Landing::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    } => Planned::Plan(LandingPlan {
      landing,
      manifest,
      summary,
      conflicts,
    }),
  }
}

/// What a landing lands: a volume's snapshot (its head when `None`), into a host directory, through a filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingRequest {
  /// The volume.
  pub volume: VolumeId,
  /// The snapshot to land, or the head.
  pub snapshot: Option<SnapshotId>,
  /// The host directory written: absolute, and outside every slates mount (§4.13).
  pub target: String,
  /// Path prefixes to keep and to leave out (`/.git` left out keeps git's own files out of the target).
  pub filter: Filter,
}

impl Instance {
  /// Attaches to the running instance `name`: the rendezvous (§4.7), with the product's derived deadlines
  /// ([`crate::deadlines`]). Refused [`Error::Unavailable`] at once when no daemon serves the instance.
  pub fn attach(name: &str) -> Result<Instance, Error> {
    let client = Client::connect(name, crate::deadlines()).map_err(|e| Error::of(name, e))?;
    Ok(Instance {
      name: name.to_owned(),
      client,
    })
  }

  /// An instance over a client the caller connected itself (the CLI's, which connects before it knows the verb).
  pub fn from_client(name: &str, client: Client) -> Instance {
    Instance {
      name: name.to_owned(),
      client,
    }
  }

  /// The instance's name.
  pub fn name(&self) -> &str {
    &self.name
  }

  /// The whole typed client, for the verbs this crate does not wrap (the merge verbs of §4.16, the file verbs).
  pub fn client(&mut self) -> &mut Client {
    &mut self.client
  }

  /// The client, given back.
  pub fn into_client(self) -> Client {
    self.client
  }

  /// A client failure of a verb to this instance, typed.
  pub(crate) fn error(&self, error: slates_client::ClientError) -> Error {
    Error::of(&self.name, error)
  }

  /// Makes this node's consensus group the root of a one-node fleet (§4.8 `bootstrap root`), or finds it made: a new
  /// instance holds no volume until its group exists. Call it only for an instance this host runs as a standalone
  /// node; a fleet node imports its groups, and a group made on a guess would let two nodes each found their own.
  pub fn ensure_group(&mut self) -> Result<(), Error> {
    let member = self.daemon_status()?.fleet.host;
    match self
      .client
      .call(&RequestBody::Bootstrap { root: true, member })
    {
      Ok(ReplyBody::Acknowledged)
      | Err(slates_client::ClientError::Refused(
        slates_ipc::protocol::Refusal::ConsensusAlreadyInitialized,
      )) => Ok(()),
      Ok(_) => Err(Error::Channel(
        slates_client::ClientError::UnexpectedReply { verb: "bootstrap" },
      )),
      Err(error) => Err(self.error(error)),
    }
  }

  /// Creates a volume (§4.4); its id. [`crate::overlay`] makes the spec of a volume laid over a host directory,
  /// [`crate::scratch`] one with no base. Refused `AlreadyExists` naming the volume that holds the name.
  pub fn create(&mut self, spec: &CreateSpec) -> Result<VolumeId, Error> {
    self.client.create(spec).map_err(|e| self.error(e))
  }

  /// Takes a snapshot of `volume`'s head (§4.4, D-5); its id.
  pub fn snapshot(&mut self, volume: VolumeId) -> Result<SnapshotId, Error> {
    self.client.snapshot(volume).map_err(|e| self.error(e))
  }

  /// Clones `snapshot` of `volume` into a new volume named `name` (§4.4, D-5); the clone's id. The clone shares every
  /// chunk with its origin until either writes.
  pub fn clone_snapshot(
    &mut self,
    volume: VolumeId,
    snapshot: SnapshotId,
    name: &str,
  ) -> Result<VolumeId, Error> {
    self
      .client
      .clone_snapshot(volume, snapshot, name)
      .map_err(|e| self.error(e))
  }

  /// Destroys a snapshot, returning what only it held to the shard; refused while a clone pins it.
  pub fn destroy_snapshot(&mut self, volume: VolumeId, snapshot: SnapshotId) -> Result<(), Error> {
    self
      .client
      .destroy_snapshot(volume, snapshot)
      .map_err(|e| self.error(e))
  }

  /// Destroys `volume` (§4.4): what it held is gone; its mounts and attachments end. The daemon tears it down in
  /// cooperative slices after it answers.
  pub fn destroy(&mut self, volume: VolumeId) -> Result<(), Error> {
    self.client.destroy(volume).map_err(|e| self.error(e))
  }

  /// The caller's volumes.
  pub fn list(&mut self) -> Result<Vec<VolumeSummary>, Error> {
    self.client.list().map_err(|e| self.error(e))
  }

  /// `volume`'s status (§4.14): its bytes, lease, attachments, head, the drift list of an overlay, its placement and
  /// its mounts.
  pub fn status(&mut self, volume: VolumeId) -> Result<StatusReport, Error> {
    self.client.status(volume).map_err(|e| self.error(e))
  }

  /// The entries of an overlay that the disk changed after the volume changed them (§4.15 "Drift"): each a path and
  /// what happened to it, as `slates status --drift` prints it. A landing of such an entry is a conflict.
  pub fn drift(&mut self, volume: VolumeId) -> Result<Vec<String>, Error> {
    Ok(self.status(volume)?.drifted)
  }

  /// Takes the disk's present state of drifted entries as their new base (all of them when `paths` is `None`, §4.15
  /// `rewitness`); the paths done. The volume's own changes to them stay its own.
  pub fn rewitness(
    &mut self,
    volume: VolumeId,
    paths: Option<Vec<String>>,
  ) -> Result<Vec<String>, Error> {
    self
      .client
      .rewitness(volume, paths)
      .map_err(|e| self.error(e))
  }

  /// Ends an attachment (a mount's, a stream's, a device's): the daemon unmounts or closes what it served.
  pub fn detach(&mut self, attachment: u64) -> Result<(), Error> {
    self.client.detach(attachment).map_err(|e| self.error(e))
  }

  /// Serves `volume` to a Shards microVM over an anonymous stream (§4.6, the Shards route; A-161): the daemon serves
  /// the attachment on its end, on the volume's owner shard, and the returned end goes to the VMM (`shards run
  /// --slates-fs-fd N` on Unix, `--slates-fs-handle` on Windows), which reaches that attachment and nothing else.
  /// With `subtree`, only that directory; with `Intent::Read`, every change refused. The attachment ends when the
  /// VMM's end closes, a [`Instance::detach`], or the volume's destroy. No host mount is involved.
  pub fn serve_shards(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    subtree: Option<&str>,
  ) -> Result<ShardsStream, Error> {
    self
      .client
      .attach_shards(volume, intent, subtree)
      .map_err(|e| self.error(e))
  }

  /// Serves `volume` through slates' own virtio-fs device to an out-of-process VMM over vhost-user (§4.6; G-vhost): the
  /// returned end goes to the VMM as its front end's socket (crosvm takes `socket=/proc/self/fd/N`); the guest mounts
  /// `tag`; `dax_window` offers a DAX window of that many bytes (a power of two of at least 2 MiB). Linux only:
  /// elsewhere refused `AttachmentUnsupported`.
  pub fn serve_vhost_user(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    (tag, subtree): (&str, Option<&str>),
    dax_window: Option<u64>,
  ) -> Result<VhostUserDevice, Error> {
    self
      .client
      .attach_vhost_user(volume, intent, (tag, subtree), dax_window)
      .map_err(|e| self.error(e))
  }

  /// Attaches `volume` in the record form (§4.4): a write intent takes the volume's write lease (D-16). What the file
  /// verbs of [`Instance::client`] need.
  pub fn attach_volume(&mut self, volume: VolumeId, intent: Intent) -> Result<Attachment, Error> {
    self
      .client
      .attach(volume, None, intent)
      .map_err(|e| self.error(e))
  }

  /// Presents the landing of `request` (§4.15 steps 1–2): with nothing needing a grant it runs as it is
  /// ([`Planned::Landed`]); otherwise its plan, for a person to approve.
  pub fn plan_landing(&mut self, request: &LandingRequest) -> Result<Planned, Error> {
    self.land_with(request, None).map(planned)
  }

  /// Every entry `plan` would write (§4.15 step 2, "the full entry list on demand"; A-287): path by path in the
  /// manifest's order, each create and replace with its bytes' BLAKE3 and length as the manifest binds them, a
  /// rename's origin, a symlink's target, the mode a file or directory lands with, and the conflict found at it. A
  /// reviewer with no mount of the volume builds the change from the list and the volume's files, checking each file it
  /// reads against its entry: a volume reads only at its head, so a mismatch means it changed since `plan`'s snapshot
  /// and the landing is presented again. Read from the presentation `plan` names, which lives until this client
  /// presents the same volume and target again, lands it, or goes: `Refused(NotFound)` after that.
  pub fn landing_entries(&mut self, plan: &LandingPlan) -> Result<Vec<PlannedEntry>, Error> {
    self
      .client
      .landing_entries(plan.landing, plan.manifest)
      .map_err(|e| self.error(e))
  }

  /// Lands `request` under `grant` (§4.15 steps 3–6): the grant must bind this landing's volume, target and consumer,
  /// or the landing is refused `GrantMismatch` before any write. A grant that does not cover the manifest planned now
  /// (the volume or the disk changed since it was granted) writes nothing and answers with the new plan.
  pub fn land(&mut self, request: &LandingRequest, grant: u64) -> Result<Planned, Error> {
    self.land_with(request, Some(grant)).map(planned)
  }

  /// Asks the daemon to issue the grant `claim` describes, under `proof` (§4.13 "Grants"); the grant id, which
  /// [`Instance::land`] then lands under. The call takes any proof form the daemon accepts ([`GrantProof`]), whoever
  /// made it: a person's out-of-band assertion as much as a [`GrantAuthority`] of this process. Refused
  /// `GrantIssuerUnverified` when the proof does not verify, `GrantMismatch` when the manifest is not the presented
  /// one, `NotFound` when nothing awaits under the landing (its presentation was replaced, or its client went away:
  /// present it again).
  pub fn grant(&mut self, claim: &GrantClaim, proof: &GrantProof) -> Result<u64, Error> {
    self
      .client
      .grant(
        claim.landing,
        claim.manifest,
        claim.scope,
        claim.term_ns,
        proof.wire(),
      )
      .map_err(|e| self.error(e))
  }

  /// Lands exactly what a person approved (§4.13 "Grants", §4.15): presents the landing again, and when its manifest
  /// is still `approved`, has `authority` prove the grant (`scope`, for `term_ns`), has it issued and lands under it;
  /// when the manifest changed, proves and writes nothing and returns the new plan to show. The grant binds the
  /// manifest the person saw, so a plan that grew since cannot ride their approval.
  pub fn land_approved(
    &mut self,
    authority: &dyn GrantAuthority,
    request: &LandingRequest,
    approved: &[u8; 32],
    (scope, term_ns): (GrantScope, u64),
  ) -> Result<Approved, Error> {
    let plan = match self.plan_landing(request)? {
      Planned::Landed(outcome) => return Ok(Approved::Landed(outcome)),
      Planned::Plan(plan) => plan,
    };
    if &plan.manifest != approved {
      return Ok(Approved::Changed(plan));
    }
    let claim = plan.claim(scope, term_ns);
    let proof = authority.prove(&claim)?;
    let grant = self.grant(&claim, &proof)?;
    match self.land(request, grant)? {
      Planned::Landed(outcome) => Ok(Approved::Landed(outcome)),
      Planned::Plan(plan) => Ok(Approved::Changed(plan)),
    }
  }

  /// One `land` call: presented without a grant, run with one.
  fn land_with(&mut self, request: &LandingRequest, grant: Option<u64>) -> Result<Landing, Error> {
    self
      .client
      .land(
        request.volume,
        request.snapshot,
        &request.target,
        request.filter.clone(),
        grant,
      )
      .map_err(|e| self.error(e))
  }

  /// The daemon's status (§4.14 `slates.status`): its shards, health signals with their freshness, the fleet's view,
  /// the transports it serves.
  pub fn daemon_status(&mut self) -> Result<DaemonReport, Error> {
    self.client.daemon_status().map_err(|e| self.error(e))
  }

  /// The audit log from record `since` (§4.15, §4.14): grants, manifests and landing outcomes, in order.
  pub fn audit(&mut self, since: u64) -> Result<Vec<AuditEntry>, Error> {
    self.client.audit(since).map_err(|e| self.error(e))
  }

  /// The caller's grants (§4.13): each with its scope, term and state.
  pub fn grants(&mut self) -> Result<Vec<GrantSummary>, Error> {
    self.client.grants().map_err(|e| self.error(e))
  }

  /// One shard's telemetry drain (§4.14): the chokepoint spans its bounded ring holds, up to one reply's quota, with
  /// the loss marker for the batch and every chokepoint's freshness. A read that consumes.
  pub fn telemetry(&mut self, partition: u16) -> Result<TelemetryReport, Error> {
    self.client.telemetry(partition).map_err(|e| self.error(e))
  }
}
