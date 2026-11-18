//! An instance as an embedding host holds it (§4.12 "Embedding", A-278; "The consumer contract", A-299): one client of
//! one running daemon, and every verb of the contract — provisioning and managing volumes (§4.4), serving them (§4.6),
//! their drift and landings (§4.15), merges (§4.16), and observing the daemon (§4.14). Each verb answers a typed
//! outcome or a typed [`Error`]; none answers text to parse.
//!
//! **One client, held.** An [`Instance`] is a connected `slates_client::Client`, so a host makes one and keeps it: a
//! landing's presentation lives as long as the client that presented it (A-46), so a plan an approver is shown is
//! approved on the same instance. The client reconnects under its own id when its daemon restarts and resends what it
//! had in flight, which the daemon answers from its completion records (§4.9), so a held instance outlives a daemon
//! restart.
//!
//! **Write permission (A-299, Ada's rule of 2026-11-18).** A landing or a merge follows its caller's write mode
//! ([`crate::WriteMode`]): bypass writes at once, ask answers [`LandOutcome::ApprovalRequired`] /
//! [`MergeOutcome::ApprovalRequired`], forbid answers its `Refused` outcome. The launcher sets a volume's mode with
//! [`Instance::set_write_policy`] under its authority (the instance's issuer); the daemon keeps and enforces it (A-285,
//! kiln agent g220): a bypass volume lands in the presenting request, a forbidding one is refused before any host
//! access, an unset one asks; a merge follows its work's and green's modes the same way. [`Instance::approve_landing`]
//! and [`Instance::approve_merge`] approve an ask with the launcher's authority, never more than the manifest or the
//! increment the approver was shown.
//!
//! **Signed work (A-321).** A merge or a landing may carry a signature over its statement — a merge's from
//! [`Instance::merge_statement`], a landing's from any presentation ([`LandingApproval::statement`]); its outcome says
//! how the daemon judged it, and the verdict is recorded: a merge's with the version ([`Instance::merge_record`]), a
//! landing's on its audit records ([`Instance::audit`]). A volume may require signed merges. The launcher sets a
//! volume's trusted signers ([`Instance::set_signers`]).

use slates_client::{
  Attachment, Client, ClientError, CreateSpec, Landing, Rebased, Rewitnessed, ShardsStream,
  SignedLanding, Submitted, VhostUserDevice,
};
use slates_ipc::protocol::{
  DaemonReport, Filter, GrantScope, Intent, MergeWindow, PlannedEntry, ReadAt, Refusal, ReplyBody,
  RequestBody, SizeClass, SnapshotId, TelemetryReport, VolumeId, VolumeSummary,
};

use crate::error::{Error, Invalid, Missing, ProtocolFault};
use crate::grant::{GrantAuthority, GrantClaim, GrantProof};
use crate::observe::{AuditRecord, GrantRecord, audit_of, grant_of};
use crate::outcome::{
  LandOutcome, LandingApproval, LandingConflict, MergeApproval, MergeConflict, MergeConflicts,
  MergeOutcome, MergeRecord, MoveProgress, RebaseOutcome, Work, merge_class_of, report_of,
  summary_of,
};
use crate::policy::{PolicyRefusal, PolicySubject, WritePolicy};
use crate::signed::{MergeStatement, TrustedSigner, Verification, WorkSignature};
use crate::volume::{
  BaseState, DriftReport, Provisioned, Source, VolumeSpec, VolumeStatus, status_of,
};

/// A client of one running instance, and every verb of the contract.
#[derive(Debug)]
pub struct Instance {
  /// The instance's name (its rendezvous, §4.7).
  pub(crate) name: String,
  /// The connected client.
  pub(crate) client: Client,
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
  /// Path prefixes to keep and to leave out, any number of each (`/.git` left out keeps git's own files out of the
  /// target).
  pub filter: Filter,
}

/// A presentation as the daemon answered it, typed: a plan to approve, or what needs none.
enum Presented {
  /// A plan with something to write and no conflict: its id, manifest and summary.
  Plan(LandingApproval),
  /// Anything else: written at once (nothing needed an approval), or conflicted.
  Settled(LandOutcome),
}

impl Presented {
  /// What the presentation answers a consumer: its plan as an approval to ask for, or what settled it.
  fn outcome(self) -> LandOutcome {
    match self {
      Presented::Plan(approval) => LandOutcome::ApprovalRequired(approval),
      Presented::Settled(outcome) => outcome,
    }
  }
}

impl Instance {
  /// Attaches to the running instance `name`: the rendezvous (§4.7), with the product's derived deadlines
  /// ([`crate::deadlines`]). Refused [`Error::Unavailable`] at once when no daemon serves the instance.
  pub fn attach(name: &str) -> Result<Instance, Error> {
    let client = Client::connect(name, crate::deadlines())
      .map_err(|error| Error::of_client(name, error, None, slates_server::daemon::HEARTBEAT_NS))?;
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

  /// The whole typed client, for the verbs the contract does not wrap (the file verbs, `edit` and `declare` on a work
  /// volume).
  pub fn client(&mut self) -> &mut Client {
    &mut self.client
  }

  /// The client, given back.
  pub fn into_client(self) -> Client {
    self.client
  }

  /// The pause before retrying a call refused for now, after `previous_ns` (zero for the first): the client's own
  /// pacing, from the daemon's published spin window doubling to a tenth of the reconnect budget. A
  /// [`Error::Retry`] carries the first.
  pub fn retry_pause_ns(&self, previous_ns: u64) -> u64 {
    self.client.retry_pause_ns(previous_ns)
  }

  /// A client failure of a call that named `subject`, typed, with the client's first retry pause.
  pub(crate) fn fail(&self, error: ClientError, subject: Option<Missing>) -> Error {
    Error::of_client(&self.name, error, subject, self.client.retry_pause_ns(0))
  }

  /// A client failure of a call that names nothing that could be missing, typed.
  pub(crate) fn error(&self, error: ClientError) -> Error {
    self.fail(error, None)
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
      | Err(ClientError::Refused(Refusal::ConsensusAlreadyInitialized)) => Ok(()),
      Ok(_) => Err(Error::Protocol(ProtocolFault::UnexpectedReply {
        verb: "bootstrap",
      })),
      Err(error) => Err(self.error(error)),
    }
  }

  /// Makes the volume `spec` names (§4.4): an overlay over a host folder, an empty volume, a clone of a snapshot, a
  /// green or a work volume over one ([`Source`]). Refused [`Error::AlreadyExists`] naming the volume that holds the
  /// name; an overlay over a relative folder is refused [`crate::Invalid::Path`] before the daemon is asked.
  pub fn provision(&mut self, spec: &VolumeSpec) -> Result<Provisioned, Error> {
    let name = spec.name.as_str();
    match &spec.source {
      Source::Overlay {
        folder,
        max_bytes,
        names,
      } => self.create(&CreateSpec {
        name: name.to_owned(),
        size: SizeClass::Dynamic { max: *max_bytes },
        names: *names,
        require_locked: false,
        base: Some(crate::absolute_text(folder)?),
      }),
      Source::Empty { limit_bytes, names } => self.create(&CreateSpec {
        name: name.to_owned(),
        size: SizeClass::Bounded {
          limit: *limit_bytes,
        },
        names: *names,
        require_locked: false,
        base: None,
      }),
      Source::Clone { volume, snapshot } => self
        .client
        .clone_snapshot(*volume, *snapshot, name)
        .map(Provisioned::Volume)
        .map_err(|e| {
          self.fail(
            e,
            Some(Missing::Snapshot {
              volume: *volume,
              snapshot: *snapshot,
            }),
          )
        }),
      Source::Green {
        base,
        require_evidence,
      } => self.create_green(name, *base, *require_evidence),
      Source::Work { green } => self
        .client
        .create_work(*green, name)
        .map(|(volume, base)| Provisioned::Work {
          work: Work {
            volume,
            green: *green,
          },
          base,
        })
        .map_err(|e| self.fail(e, Some(Missing::Volume(*green)))),
    }
  }

  /// One `create`.
  fn create(&mut self, spec: &CreateSpec) -> Result<Provisioned, Error> {
    self
      .client
      .create(spec)
      .map(Provisioned::Volume)
      .map_err(|e| self.error(e))
  }

  /// One green, empty or over a snapshot.
  fn create_green(
    &mut self,
    name: &str,
    base: Option<slates_ipc::protocol::GreenBase>,
    require_evidence: bool,
  ) -> Result<Provisioned, Error> {
    match base {
      None => self
        .client
        .create_green(name, require_evidence)
        .map(Provisioned::Volume)
        .map_err(|e| self.error(e)),
      Some(base) => self
        .client
        .create_green_over(name, require_evidence, base)
        .map(Provisioned::Volume)
        .map_err(|e| {
          self.fail(
            e,
            Some(Missing::Snapshot {
              volume: base.volume,
              snapshot: base.snapshot,
            }),
          )
        }),
    }
  }

  /// Takes a snapshot of `volume`'s head (§4.4, D-5); its id.
  pub fn snapshot(&mut self, volume: VolumeId) -> Result<SnapshotId, Error> {
    self
      .client
      .snapshot(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// Captures an overlay's whole base into a complete immutable snapshot (§4.15 `capture_base`, A-291), its id: every
  /// file's bytes and every link's target become the volume's own, so a clone of the snapshot
  /// (`VolumeSpec::clone_of`) needs no base directory, survives its runner's loss and moves between fleet members
  /// ([`Instance::move_volume`]). The host asserts nothing writes the folder while the capture runs (kiln: a commit's
  /// tree checked out where nothing else writes); a file that changed while it was read is refused `Error::Retry`
  /// (`RetryCause::ChangedWhileRead`) and a folder that moved under the capture [`crate::Invalid::LiveBase`], and the
  /// capture may be asked again. It reads the whole folder, so it takes as long as that does; a volume that is not an
  /// overlay is refused [`crate::Unsupported::Feature`].
  pub fn capture_base(&mut self, volume: VolumeId) -> Result<SnapshotId, Error> {
    self
      .client
      .capture_base(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// Exports `snapshot` of `volume` as its archive (§4.11 "Archive", D-17; Mantle's CP-T10.4, A-441): a new volume
  /// `name`, the caller's own, whose root holds one file, [`crate::ARCHIVE_FILE`], the snapshot's content-addressed
  /// archive. Read it a page at a time with [`Instance::read`] and keep it wherever bytes are kept (an object store),
  /// then destroy the volume. The archive carries every file's bytes and every entry's metadata, verified by identity
  /// on the way back in; its `manifest` is the tree's identity, the same for the same tree wherever it is exported. It
  /// waits for the export, which walks the snapshot in slices on its owner. `read` on the volume is required; a
  /// snapshot whose entries still read an overlay's base is refused [`crate::Unsupported::Feature`] (capture the base
  /// first, [`Instance::capture_base`], and export the captured snapshot); a merge volume's is refused too.
  pub fn export_archive(
    &mut self,
    volume: VolumeId,
    snapshot: SnapshotId,
    name: &str,
  ) -> Result<crate::ArchiveExported, Error> {
    self
      .client
      .archive_export(volume, snapshot, name)
      .map_err(|e| self.fail(e, Some(Missing::Snapshot { volume, snapshot })))
  }

  /// Destroys a snapshot, returning what only it held to the shard; refused while a clone pins it.
  pub fn destroy_snapshot(&mut self, volume: VolumeId, snapshot: SnapshotId) -> Result<(), Error> {
    self
      .client
      .destroy_snapshot(volume, snapshot)
      .map_err(|e| self.fail(e, Some(Missing::Snapshot { volume, snapshot })))
  }

  /// Destroys `volume` (§4.4): what it held is gone; its mounts and attachments end. The daemon tears it down in
  /// cooperative slices after it answers.
  pub fn destroy(&mut self, volume: VolumeId) -> Result<(), Error> {
    self
      .client
      .destroy(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// The caller's volumes, in name order: every one, walked a page at a time (A-263; [`Instance::list_page`] reads one
  /// page, for a consumer that shows them a screen at a time).
  pub fn list(&mut self) -> Result<Vec<VolumeSummary>, Error> {
    self.client.list().map_err(|e| self.error(e))
  }

  /// `volume`'s status (§4.14), typed: its bytes, lease, attachments, head, placement, transports, mounts, journal, and
  /// an overlay's base with its drift.
  pub fn status(&mut self, volume: VolumeId) -> Result<VolumeStatus, Error> {
    self
      .client
      .status(volume)
      .map(status_of)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// An overlay's drift (§4.15 "Drift"): each entry the disk changed after the volume changed it, typed by what
  /// happened — landing it would conflict — and each whose re-check the host refused now. `None` for a volume with no
  /// base. Refused [`crate::HostFault::BaseUnreadable`] when the base could not be read now. Every entry: what a status
  /// could not carry is read a page at a time (A-267, [`Instance::drift_page`]), and [`DriftReport::elided`] then
  /// counts only the entries too long for any reply.
  pub fn drift(&mut self, volume: VolumeId) -> Result<Option<DriftReport>, Error> {
    let report = match self.status(volume)?.base {
      None => return Ok(None),
      Some(BaseState::Read(report)) => report,
      Some(BaseState::Unreadable) => {
        return Err(Error::Host(crate::error::HostFault::BaseUnreadable));
      }
    };
    if report.elided == 0 {
      return Ok(Some(report));
    }
    let mut whole = DriftReport {
      drifted: Vec::new(),
      unverified: Vec::new(),
      watcher: report.watcher,
      elided: 0,
    };
    let mut after = None;
    loop {
      let page = self.drift_page(volume, after)?;
      whole.drifted.extend(page.drifted);
      whole.unverified.extend(page.unverified);
      whole.elided = whole.elided.saturating_add(page.oversized);
      match page.next {
        None => {
          // The pages come in inode order; the report names its entries in path order, as a status does.
          whole.drifted.sort_by(|a, b| a.path.cmp(&b.path));
          whole.unverified.sort();
          return Ok(Some(whole));
        }
        next => after = next,
      }
    }
  }

  /// A file's bytes in `volume` (§4.4): at its head, at a green's version, or through an attachment's pinned view
  /// ([`ReadAt`]). A reviewer reads each entry an approval lists at the head and checks it against the entry's hash
  /// ([`Instance::approval_entries`]); a mismatch means the volume changed since, and the landing is presented again.
  pub fn read(&mut self, volume: VolumeId, path: &str, at: ReadAt) -> Result<Vec<u8>, Error> {
    self.client.read(volume, path, at).map_err(|e| {
      self.fail(
        e,
        Some(Missing::Entry {
          volume,
          path: path.to_owned(),
        }),
      )
    })
  }

  /// Takes the disk's present state of drifted entries as their new base (all of them when `paths` is `None`, §4.15
  /// `rewitness`); the paths done, as many as one reply holds, and the count of the rest (A-267), which were taken
  /// too. The volume's own changes to them stay its own.
  pub fn rewitness(
    &mut self,
    volume: VolumeId,
    paths: Option<Vec<String>>,
  ) -> Result<Rewitnessed, Error> {
    self
      .client
      .rewitness(volume, paths)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// Ends an attachment (a mount's, a stream's, a device's): the daemon unmounts or closes what it served.
  pub fn detach(&mut self, attachment: u64) -> Result<(), Error> {
    self
      .client
      .detach(attachment)
      .map_err(|e| self.fail(e, Some(Missing::Attachment { attachment })))
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
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// Serves `volume` through slates' own virtio-fs device to an out-of-process VMM over vhost-user (§4.6; G-vhost): the
  /// returned end goes to the VMM as its front end's socket (crosvm takes `socket=/proc/self/fd/N`); the guest mounts
  /// `tag`; `dax_window` offers a DAX window of that many bytes (a power of two of at least 2 MiB). Linux only:
  /// elsewhere refused `Unsupported(Attachment)`.
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
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// Attaches `volume` in the record form (§4.4): a write intent takes the volume's write lease (D-16). What the file
  /// verbs of [`Instance::client`] need.
  pub fn attach_volume(&mut self, volume: VolumeId, intent: Intent) -> Result<Attachment, Error> {
    self
      .client
      .attach(volume, None, intent)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// Lands `request` under the caller's write mode (§4.15): [`LandOutcome::Written`] when it runs,
  /// [`LandOutcome::ApprovalRequired`] when the mode asks (what would be written, for [`Instance::approve_landing`]),
  /// [`LandOutcome::Conflicted`] when the disk changed entries the volume changed too, [`LandOutcome::Refused`] when a
  /// policy forbids it. A `signature` over the landing's statement ([`LandingApproval::statement`], which every
  /// presentation states: [`Instance::plan_landing`]) makes the landing signed work (A-321): the daemon judges it under
  /// the volume's trusted signers against the manifest it presents or writes, records the verdict on the landing's audit
  /// records ([`Instance::audit`]) and answers it — in the report of what was written, or in the approval asked for,
  /// which carries the signature on to [`Instance::approve_landing`]. A signature permits nothing: the mode decides.
  pub fn land(
    &mut self,
    request: &LandingRequest,
    signature: Option<&WorkSignature>,
  ) -> Result<LandOutcome, Error> {
    let landing = self.land_with(request, None, signature);
    self
      .presented(request, signature, landing)
      .map(Presented::outcome)
  }

  /// Presents the landing of `request` without writing it, whatever the caller's write mode (§4.15 steps 1–2; the
  /// daemon's present-only verb, A-285): the plan as an approval (its manifest, its summary, and the statement a signer
  /// signs to make the landing signed work, A-321), or the conflicts found before any write. What a reviewer asks
  /// before approving: [`Instance::approval_entries`] lists the plan path by path. A forbidding mode presents too, since
  /// a reviewer reads changes it may not land.
  pub fn plan_landing(&mut self, request: &LandingRequest) -> Result<LandOutcome, Error> {
    let presented = self.present_only(request, None);
    self
      .presented(request, None, presented)
      .map(Presented::outcome)
  }

  /// Every entry an approved landing would write (§4.15 step 2, "the full entry list on demand"; A-287): path by path
  /// in the manifest's order, each create and replace with its bytes' BLAKE3 and length as the manifest binds them, a
  /// rename's origin, a symlink's target, the mode a file or directory lands with, and the conflict found at it. A
  /// reviewer with no mount of the volume builds the change from the list and the volume's files, checking each file it
  /// reads against its entry: a volume reads only at its head, so a mismatch means it changed since the approval's
  /// snapshot and the landing is presented again. Read from the presentation the approval names, which lives until
  /// this client presents the same volume and target again, lands it, or goes: [`Missing::Presentation`] after that.
  pub fn approval_entries(
    &mut self,
    approval: &LandingApproval,
  ) -> Result<Vec<PlannedEntry>, Error> {
    self
      .client
      .landing_entries(approval.presentation, approval.manifest)
      .map_err(|e| {
        self.fail(
          e,
          Some(Missing::Presentation {
            id: approval.presentation,
          }),
        )
      })
  }

  /// Approves exactly the landing `approval` showed (§4.13 "Grants", §4.15), with the launcher's `authority`:
  /// presents the landing again (never writing), and when its manifest is still the approved one, has the authority
  /// prove a grant of that manifest for one landing, and lands under it — [`LandOutcome::Written`]. When the volume or
  /// the disk changed since, nothing is approved or written, and the answer is the new presentation
  /// ([`LandOutcome::ApprovalRequired`] to show instead, or [`LandOutcome::Conflicted`]). An approval never covers more
  /// than the manifest its approver saw, however long they took, and it works from any instance: it is bound to the
  /// request and the manifest, never to the client that first presented it. The approval's signature
  /// ([`LandingApproval::signature`], A-321) goes with the presentation and the landing, so the landing written is
  /// recorded and answered with its verdict; it permits nothing.
  pub fn approve_landing(
    &mut self,
    approval: &LandingApproval,
    authority: &dyn GrantAuthority,
  ) -> Result<LandOutcome, Error> {
    let request = &approval.request;
    let signature = approval.signature.as_ref();
    let presented = self.present_only(request, signature);
    let fresh = match self.presented(request, signature, presented)? {
      Presented::Plan(fresh) => fresh,
      Presented::Settled(outcome) => return Ok(outcome),
    };
    if fresh.manifest != approval.manifest {
      return Ok(LandOutcome::ApprovalRequired(fresh));
    }
    let claim = GrantClaim {
      landing: fresh.presentation,
      manifest: fresh.manifest,
      scope: GrantScope::Once,
      term_ns: crate::GRANT_TERM_NS,
    };
    let proof = authority.prove(&claim)?;
    let grant = self.grant(&claim, &proof)?;
    let landed = self.land_with(request, Some(grant), signature);
    self.granted(request, (fresh.presentation, signature), landed)
  }

  /// A granted landing's reply, typed: written, with how its signature was judged; refused by a policy; or, when the
  /// grant does not cover what is planned now or the disk moved between the grant and the landing's validation, nothing
  /// written and what holds now presented, carrying `signature` again.
  fn granted(
    &mut self,
    request: &LandingRequest,
    (presentation, signature): (u64, Option<&WorkSignature>),
    reply: Result<SignedLanding, ClientError>,
  ) -> Result<LandOutcome, Error> {
    match reply {
      Ok(SignedLanding {
        landing: Landing::Landed(outcome),
        ..
      }) => report_of(outcome).map(LandOutcome::Written),
      Err(ClientError::Refused(Refusal::PolicyForbids { act, by })) => {
        Ok(LandOutcome::Refused(PolicyRefusal::forbidden(act, by)))
      }
      Ok(SignedLanding {
        landing: Landing::GrantRequired { .. },
        ..
      })
      | Err(ClientError::Refused(Refusal::GrantMismatch | Refusal::LandingConflict { .. })) => {
        let again = self.present_only(request, signature);
        self
          .presented(request, signature, again)
          .map(Presented::outcome)
      }
      Err(error) => Err(self.fail(error, Some(Missing::Presentation { id: presentation }))),
    }
  }

  /// Asks the daemon to issue the grant `claim` describes, under `proof` (§4.13 "Grants"); the grant id. The call takes
  /// any proof form the daemon accepts ([`GrantProof`]), whoever made it, and any client may issue a grant for a
  /// presentation another client made; the presenting client then lands under it. Refused [`crate::Denied::Unproven`]
  /// when the proof does not verify, [`crate::Stale::Approval`] when the manifest is not the presented one, and
  /// [`Missing::Presentation`] when nothing awaits under the id (it was replaced, or its client went away).
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
      .map_err(|e| self.fail(e, Some(Missing::Presentation { id: claim.landing })))
  }

  /// A presentation's reply, typed: a plan to approve — how `signature` was judged against it, its statement, and the
  /// signature carried on for its approval (A-321) — conflicts, written at once with its verdict, or refused by a policy.
  fn presented(
    &mut self,
    request: &LandingRequest,
    signature: Option<&WorkSignature>,
    reply: Result<SignedLanding, ClientError>,
  ) -> Result<Presented, Error> {
    match reply {
      Ok(SignedLanding {
        landing: Landing::Landed(outcome),
        ..
      }) => report_of(outcome).map(|report| Presented::Settled(LandOutcome::Written(report))),
      Ok(SignedLanding {
        landing:
          Landing::GrantRequired {
            landing,
            manifest,
            summary,
            conflicts,
            conflicts_elided,
          },
        verification,
        statement: Some(statement),
      }) => {
        let approval = LandingApproval {
          request: request.clone(),
          presentation: landing,
          manifest,
          summary: summary_of(&summary),
          verification: Verification::of_wire(verification),
          statement,
          signature: signature.cloned(),
        };
        // A reply carries the conflicts one reply holds and counts the rest (A-267): clean only when it has neither.
        if conflicts.is_empty() && conflicts_elided == 0 {
          Ok(Presented::Plan(approval))
        } else {
          self.conflicts_of(&approval).map(Presented::Settled)
        }
      }
      // The client states every presentation's statement (`slates_client::SignedLanding`); a presentation without one
      // is a reply the verb does not expect.
      Ok(SignedLanding {
        landing: Landing::GrantRequired { .. },
        statement: None,
        ..
      }) => Err(Error::Protocol(ProtocolFault::UnexpectedReply {
        verb: "land",
      })),
      Err(ClientError::Refused(Refusal::PolicyForbids { act, by })) => Ok(Presented::Settled(
        LandOutcome::Refused(PolicyRefusal::forbidden(act, by)),
      )),
      Err(error) => Err(self.fail(error, Some(Missing::Volume(request.volume)))),
    }
  }

  /// A conflicted presentation's conflicts, typed, read from its entries (§4.15 step 2; A-287): each entry the
  /// preliminary verdict found in conflict, with its class and the bytes the volume would land there.
  fn conflicts_of(&mut self, approval: &LandingApproval) -> Result<LandOutcome, Error> {
    let conflicts = self
      .approval_entries(approval)?
      .into_iter()
      .filter_map(|entry| {
        entry.conflict.map(|class| LandingConflict {
          path: entry.path,
          class,
          planned: entry.content,
        })
      })
      .collect();
    Ok(LandOutcome::Conflicted(conflicts))
  }

  /// One presentation of `request` that never writes, whatever the mode (the daemon's `Present`, A-285), carrying
  /// `signature` to be judged against what it presents (A-321).
  fn present_only(
    &mut self,
    request: &LandingRequest,
    signature: Option<&WorkSignature>,
  ) -> Result<SignedLanding, ClientError> {
    self.client.present_with_signature(
      (request.volume, request.snapshot),
      &request.target,
      request.filter.clone(),
      signature.map(WorkSignature::blob),
    )
  }

  /// One `land` call carrying `signature` (A-321): presented without a grant, run with one.
  fn land_with(
    &mut self,
    request: &LandingRequest,
    grant: Option<u64>,
    signature: Option<&WorkSignature>,
  ) -> Result<SignedLanding, ClientError> {
    self.client.land_with_signature(
      (request.volume, request.snapshot),
      &request.target,
      request.filter.clone(),
      grant,
      signature.map(WorkSignature::blob),
    )
  }

  /// Merges `work` into its green under the caller's write mode (§4.16, D-27): [`MergeOutcome::Merged`] at the green's
  /// new version, [`MergeOutcome::Conflicted`] with every conflict window typed and the green's head (rebase, resolve,
  /// merge again), [`MergeOutcome::ApprovalRequired`] when the mode asks (the increment's identity, for
  /// [`Instance::approve_merge`]), [`MergeOutcome::Refused`] when a policy forbids it. `evidence` is what a green that
  /// requires evidence takes (§4.16; empty for one that does not). A `signature` over the merge's statement
  /// ([`Instance::merge_statement`]) makes the merge signed work (A-321): the outcome says how the daemon judged it, and a
  /// work or a green that requires signed merges refuses one that did not verify when it would merge
  /// ([`crate::PolicyReason::SignatureRequired`]). A signature permits nothing: the mode decides.
  pub fn merge(
    &mut self,
    work: &Work,
    evidence: &[[u8; 32]],
    signature: Option<&WorkSignature>,
  ) -> Result<MergeOutcome, Error> {
    let submitted =
      self
        .client
        .submit_with_signature(work.volume, evidence, signature.map(WorkSignature::blob));
    self.merged(work, (evidence, signature), submitted)
  }

  /// The statement a signature over `work`'s merge signs now (A-321): the work, its green, its base and the increment's
  /// identity, as the daemon states it. The work is submitted held, so nothing is merged whatever the mode; a mode that
  /// forbids the merge is refused [`crate::Denied::Policy`]. Sign it under the namespace `slates-work` (`ssh-keygen -Y
  /// sign -n slates-work -f KEY FILE`) and merge with the signature before the work changes; once it changes, the
  /// signature no longer verifies.
  pub fn merge_statement(
    &mut self,
    work: &Work,
    evidence: &[[u8; 32]],
  ) -> Result<MergeStatement, Error> {
    let stated = self.client.merge_statement(work.volume, evidence);
    stated
      .map(|stated| MergeStatement {
        work: *work,
        identity: stated.identity,
        statement: stated.statement,
      })
      .map_err(|e| self.fail(e, Some(Missing::Volume(work.volume))))
  }

  /// Merges exactly the increment `approval` showed (A-285), with the launcher's `authority`: the daemon merges it only
  /// while the work's increment is still that identity, carrying the signature the merge carried (A-321). When the work
  /// changed since, nothing is merged and the merge is asked again: the answer is what the work's mode gives now (a fresh
  /// [`MergeOutcome::ApprovalRequired`] to show instead, or its conflicts).
  pub fn approve_merge(
    &mut self,
    approval: &MergeApproval,
    authority: &dyn GrantAuthority,
  ) -> Result<MergeOutcome, Error> {
    let work = approval.work;
    let signature = approval.signature.as_ref();
    let proof = authority.prove_merge(work.volume, &approval.identity)?;
    let submitted = self.client.submit_approved_signed(
      work.volume,
      &approval.evidence,
      (approval.identity, proof.wire()),
      signature.map(WorkSignature::blob),
    );
    match submitted {
      // The work's increment is no longer the approved one: nothing was merged; what holds now is asked again.
      Err(ClientError::Refused(Refusal::GrantMismatch)) => {
        self.merge(&work, &approval.evidence, signature)
      }
      other => self.merged(&work, (&approval.evidence, signature), other),
    }
  }

  /// A submit's reply, typed, with how its signature was judged.
  fn merged(
    &mut self,
    work: &Work,
    (evidence, signature): (&[[u8; 32]], Option<&WorkSignature>),
    reply: Result<(Submitted, slates_ipc::protocol::Verification), ClientError>,
  ) -> Result<MergeOutcome, Error> {
    match reply {
      Ok((Submitted::Accepted(version), verification)) => Ok(MergeOutcome::Merged {
        version,
        verification: Verification::of_wire(verification),
      }),
      Ok((Submitted::Conflict(windows), _)) => self
        .merge_conflicts(work, windows)
        .map(MergeOutcome::Conflicted),
      Ok((Submitted::ApprovalRequired { identity, paths }, verification)) => {
        Ok(MergeOutcome::ApprovalRequired(MergeApproval {
          work: *work,
          evidence: evidence.to_vec(),
          identity,
          paths,
          verification: Verification::of_wire(verification),
          signature: signature.cloned(),
        }))
      }
      Err(ClientError::Refused(Refusal::PolicyForbids { act, by })) => {
        Ok(MergeOutcome::Refused(PolicyRefusal::forbidden(act, by)))
      }
      Err(ClientError::Refused(Refusal::SignatureRequired { by, verification })) => Ok(
        MergeOutcome::Refused(PolicyRefusal::signature_required(by, verification)),
      ),
      Err(error) => Err(self.fail(error, Some(Missing::Volume(work.volume)))),
    }
  }

  /// A green version's merge record (§4.16, A-321): how the signature of the merge that made it was judged when it
  /// merged — its provenance, kept with the green's accepted record. Version 0 is the green's origin, which no merge made
  /// (unsigned); a version not yet committed is [`Missing::Version`].
  pub fn merge_record(&mut self, green: VolumeId, version: u64) -> Result<MergeRecord, Error> {
    let read = self.client.merge_record(green, version);
    read
      .map(|record| MergeRecord {
        version: record.version,
        verification: Verification::of_wire(record.verification),
      })
      .map_err(|e| self.fail(e, Some(Missing::Version { green, version })))
  }

  /// Sets `volume`'s trusted signers (A-321): the whole set of keys whose signatures over its work verify — a merge's
  /// under its work's and its green's together — replacing the last; set none to trust none. The launcher's verb,
  /// proven by its authority (`launcher`), refused on a consumer's own channel ([`crate::Denied::Rights`]) and without the
  /// proof ([`crate::Denied::Unproven`]), as a write policy is. It names the policy version it replaces, so a setting
  /// that raced another is [`crate::Stale::Policy`].
  pub fn set_signers(
    &mut self,
    volume: VolumeId,
    signers: &[TrustedSigner],
    launcher: &crate::Issuer,
  ) -> Result<(), Error> {
    let keys: Vec<Vec<u8>> = signers
      .iter()
      .map(|signer| signer.wire().to_vec())
      .collect();
    let (_, version) = self
      .client
      .signers(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    let proof = launcher.signers_proof(volume, &keys, version)?;
    self
      .client
      .set_signers(volume, (&keys, version), proof)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))
  }

  /// `volume`'s trusted signers (A-321), in the order the launcher set them; none while none is set.
  pub fn signers(&mut self, volume: VolumeId) -> Result<Vec<TrustedSigner>, Error> {
    let (keys, _) = self
      .client
      .signers(volume)
      .map_err(|e| self.fail(e, Some(Missing::Volume(volume))))?;
    keys
      .into_iter()
      .map(|key| {
        TrustedSigner::from_wire(key).map_err(|_| {
          Error::Protocol(ProtocolFault::UnknownName {
            field: "trusted signer",
          })
        })
      })
      .collect()
  }

  /// Moves `work` onto its green's head (§4.16 "Rebase, the only corrective path"): [`RebaseOutcome::Rebased`] at the
  /// head when every pending operation maps cleanly, [`RebaseOutcome::Conflicted`] with the windows to resolve
  /// otherwise. The green is unchanged either way.
  pub fn rebase(&mut self, work: &Work) -> Result<RebaseOutcome, Error> {
    match self.client.rebase(work.volume) {
      Ok(Rebased::Rebased(version)) => Ok(RebaseOutcome::Rebased { version }),
      Ok(Rebased::Conflict(windows)) => self
        .merge_conflicts(work, windows)
        .map(RebaseOutcome::Conflicted),
      Err(error) => Err(self.fail(error, Some(Missing::Volume(work.volume)))),
    }
  }

  /// A verdict's windows, typed, with the green's head read right after.
  fn merge_conflicts(
    &mut self,
    work: &Work,
    windows: Vec<MergeWindow>,
  ) -> Result<MergeConflicts, Error> {
    let conflicts = windows
      .into_iter()
      .map(|window| {
        merge_class_of(window.class).map(|class| MergeConflict {
          path: window.path,
          at: window.at,
          len: window.len,
          class,
        })
      })
      .collect::<Result<Vec<_>, Error>>()?;
    let head = self
      .client
      .versions(work.green)
      .map_err(|e| self.fail(e, Some(Missing::Volume(work.green))))?;
    Ok(MergeConflicts { conflicts, head })
  }

  /// Sets `subject`'s write policy (A-299, A-285): the launcher's verb, proven by its authority (`launcher`, the
  /// instance's issuer). The daemon refuses it on a consumer's own channel whatever the proof, so an agent never sets
  /// or raises its own mode ([`crate::Denied::Rights`]); a proof that does not verify is [`crate::Denied::Unproven`]. A
  /// volume's policy may require signed merges (A-321); a consumer's that does is refused
  /// [`Invalid::SignedMergesNotAVolume`] before anything is sent, since the requirement is a volume's rule.
  pub fn set_write_policy(
    &mut self,
    subject: PolicySubject,
    policy: &WritePolicy,
    launcher: &crate::Issuer,
  ) -> Result<(), Error> {
    if policy.require_signed_merges && matches!(subject, PolicySubject::Consumer(_)) {
      return Err(Error::Invalid(Invalid::SignedMergesNotAVolume));
    }
    let wire_policy = policy.wire();
    let wire_subject = subject.wire();
    // The setting names the version it replaces (A-285): read it, then set from it. One raced in between is refused
    // `Stale::Policy`, and nothing changes.
    let (_, version) = self
      .client
      .policy(wire_subject)
      .map_err(|e| self.fail(e, Some(missing_subject(subject))))?;
    let proof = launcher.policy_proof(&wire_subject, &wire_policy, version)?;
    self
      .client
      .set_policy(wire_subject, (wire_policy, version), proof)
      .map_err(|e| self.fail(e, Some(missing_subject(subject))))
  }

  /// `subject`'s write policy as the daemon holds it, or `None` while nobody has set one, which merges at once and
  /// asks for landings ([`WritePolicy::UNSET`]).
  pub fn write_policy(&mut self, subject: PolicySubject) -> Result<Option<WritePolicy>, Error> {
    self
      .client
      .policy(subject.wire())
      .map(|(policy, _)| policy.map(WritePolicy::of_wire))
      .map_err(|e| self.fail(e, Some(missing_subject(subject))))
  }

  /// Asks `volume`'s owner to move it to the fleet member `to` (§4.10 "Ownership follows the writer", A-291), the
  /// member named by its id ([`Instance::member`] on that member's own instance). One ask: the move goes on between
  /// asks, so a consumer asks again until it is [`MoveProgress::Moved`], and the volume is then served by `to` once it
  /// has adopted it (its verbs answer [`crate::RetryCause::Overloaded`] until then). The volume must be a plain one with
  /// no attachment, clone or snapshot but its head, and `admin` on it is required. Each refusal is typed by what to do:
  /// [`crate::Busy::Attached`] (detach first), [`crate::Busy::Moving`], [`crate::Unusable::Unmovable`],
  /// [`crate::Invalid::MoveTarget`], [`crate::RetryCause::MoveUnplaced`]; a lone instance, which has no other member, is
  /// refused [`crate::MoveTarget::NotMember`].
  pub fn move_volume(&mut self, volume: VolumeId, to: u64) -> Result<MoveProgress, Error> {
    match self.client.move_volume(volume, to) {
      Ok(slates_client::MoveState::Moving { to }) => Ok(MoveProgress::Moving { to }),
      Ok(slates_client::MoveState::Moved { to }) => Ok(MoveProgress::Moved { to }),
      Err(e) => Err(self.fail(e, Some(Missing::Volume(volume)))),
    }
  }

  /// This instance's fleet member id: what another instance's [`Instance::move_volume`] names it by, and what its
  /// volume ids and its peers name it by (§4.8). It is the node's certificate anchor with its boot's nonce, so a
  /// daemon restarted under the same anchor is a new member; ask it again after a restart.
  pub fn member(&mut self) -> Result<u64, Error> {
    Ok(self.daemon_status()?.fleet.host)
  }

  /// The daemon's status (§4.14 `slates.status`): its shards, health signals with their freshness, the fleet's view,
  /// the transports it serves.
  pub fn daemon_status(&mut self) -> Result<DaemonReport, Error> {
    self.client.daemon_status().map_err(|e| self.error(e))
  }

  /// The audit log from record `since` (§4.15, §4.14), typed: grants, presentations, validations, entries and
  /// landings finished, in order; each record of a landing as a whole (planned, finished) with how its signature was
  /// judged (A-321), the landing's provenance.
  pub fn audit(&mut self, since: u64) -> Result<Vec<AuditRecord>, Error> {
    Ok(
      self
        .client
        .audit(since)
        .map_err(|e| self.error(e))?
        .into_iter()
        .map(audit_of)
        .collect(),
    )
  }

  /// The caller's grants (§4.13), typed: each with its volume, target, manifest, scope and state.
  pub fn grants(&mut self) -> Result<Vec<GrantRecord>, Error> {
    Ok(
      self
        .client
        .grants()
        .map_err(|e| self.error(e))?
        .into_iter()
        .map(grant_of)
        .collect(),
    )
  }

  /// One shard's telemetry drain (§4.14): the chokepoint spans its bounded ring holds, up to one reply's quota, with
  /// the loss marker for the batch and every chokepoint's freshness. A read that consumes.
  pub fn telemetry(&mut self, partition: u16) -> Result<TelemetryReport, Error> {
    self
      .client
      .telemetry(partition)
      .map_err(|e| self.fail(e, Some(Missing::Shard { shard: partition })))
  }
}

/// What a policy verb names, as a missing subject.
fn missing_subject(subject: PolicySubject) -> Missing {
  match subject {
    PolicySubject::Consumer(consumer) => Missing::Consumer { consumer },
    PolicySubject::Volume(volume) => Missing::Volume(volume),
  }
}
