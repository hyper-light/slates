//! The client: the rendezvous, one request in flight, exactly-once retries, typed verbs.

#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::time::Instant;

use slates_ipc::delivery::{Capability, Delivered, DeliveryFault, attest_proof};
use slates_ipc::protocol::{
  AttachRequest, AttachmentCapability, AuditEntry, DaemonReport, Direction, Established, Filter,
  GrantScope, GrantSummary, GreenBase, Intent, LandingOutcome, LandingSummary, MergeWindow,
  NamePolicy, Principal, ReadAt, ReplyBody, RequestBody, Rights, Scope, SizeClass,
  SnapshotCoverage, SnapshotId, StatusReport, TelemetryReport, VolumeId, VolumeSummary, WorkOp,
  pack, unpack,
};
use slates_ipc::{
  CLAIM_WAIT_NS, Claim, ClientEnd, Connected, IpcError, begin_connect_as, connect_as,
};
use slates_machine::{Derived, derived};
use slates_wire::request::RequestId;

use crate::error::ClientError;

/// The result of a submit (§4.16): accepted at a new green version, or a conflict with the windows
/// to rebase against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Submitted {
  /// The increment merged; the green's new head version.
  Accepted(u64),
  /// The increment conflicts; the windows to rebase against.
  Conflict(Vec<MergeWindow>),
}

/// The result of a rebase (§4.16 "Rebase, the only corrective path"): the work moved onto the head
/// version, or a conflict with the windows to resolve first. The green is unchanged either way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rebased {
  /// Every operation mapped cleanly; the head version the work is now based on.
  Rebased(u64),
  /// The pending operations conflict; the windows to resolve, then rebase again.
  Conflict(Vec<MergeWindow>),
}

/// The client's deadlines, every one a derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
  /// How long a reply is waited for before the daemon's liveness is questioned.
  pub reply_ns: u64,
  /// How long a reconnect is tried after the daemon is found gone.
  pub reconnect_ns: u64,
}

impl Deadlines {
  /// Derived: the reply deadline is the anchor's liveness budget (a daemon silent longer than
  /// that is presumed dead by its own supervisor, so the client asks the same question then);
  /// the reconnect budget is the recovery budget the restart is bounded by, plus one reply
  /// deadline for the restarted daemon's first answer.
  pub fn derive(liveness_budget_ns: u64, recovery_budget_ns: u64) -> Derived<Deadlines> {
    derived!(
      Deadlines {
        reply_ns: liveness_budget_ns,
        reconnect_ns: recovery_budget_ns.saturating_add(liveness_budget_ns),
      },
      "reply_ns = liveness_budget_ns; reconnect_ns = recovery_budget_ns + liveness_budget_ns",
      ["liveness_budget_ns", "recovery_budget_ns"]
    )
  }
}

/// Whether the daemon defers `body`'s reply until long work ends — a granted landing, which runs in slices for
/// as long as its tree takes, and a FUSE mount, which waits on the OS's mount helper — so a caller waits for
/// it while the daemon lives rather than for one reply deadline.
pub fn defers_reply(body: &RequestBody) -> bool {
  matches!(
    body,
    RequestBody::Land { grant: Some(_), .. }
      | RequestBody::Attach {
        form: AttachRequest::FuseMount { .. }
          | AttachRequest::ScopedFuseMount { .. }
          | AttachRequest::SharedFuseMount { .. },
        ..
      }
  )
}

/// Derived: the last sequence a client issues. Sequences run from 1 up to here and never wrap: a wrapped
/// sequence would meet the daemon's window as already acknowledged, or meet an earlier completion (AUD-29-21).
/// `u32::MAX` is kept as the [`Session::next_sequence`] of a client that issued this one — a resume from it
/// issues nothing, so the last id is never reissued. Past it every fresh request is refused
/// `SequencesExhausted`, and the transition is a new client: a fresh client id and window, while retries of
/// ids already issued still reach their completion records.
pub const LAST_SEQUENCE: u32 = u32::MAX - 1;

/// What a client keeps to resume: its id and the next sequence it would use. A process that
/// restarts with this retries what it had in flight and meets the completion record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
  /// The client id the daemon bound to this principal.
  pub client_id: u32,
  /// The next sequence.
  pub next_sequence: u32,
}

/// What `create` takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateSpec {
  /// The name.
  pub name: String,
  /// The size class.
  pub size: SizeClass,
  /// The name policy.
  pub names: NamePolicy,
  /// Whether unlocked memory refuses.
  pub require_locked: bool,
  /// A host directory to overlay, or nothing for a scratch volume.
  pub base: Option<String>,
}

/// What a `land` call returns: the landing ran, or it needs a grant a human must make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landing {
  /// The landing finished (or partially).
  Landed(LandingOutcome),
  /// A grant is required first; the manifest the human must see.
  GrantRequired {
    /// The landing id `slates grant` names.
    landing: u64,
    /// The manifest hash the grant must bind.
    manifest: [u8; 32],
    /// The summary of what would be written.
    summary: LandingSummary,
    /// Conflicts found before any write.
    conflicts: Vec<String>,
  },
}

/// What `digest` returns (§4.15): a clean base file's verified content digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Digest {
  /// BLAKE3 of the file's bytes as the disk holds them.
  pub identity: [u8; 32],
  /// The length digested, in bytes.
  pub size: u64,
}

/// What `attach` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
  /// The attachment id (what `detach` takes).
  pub attachment: u64,
  /// The lease epoch, for a write attachment.
  pub lease_epoch: Option<u64>,
  /// The path a bridge publishes (none until a bridge exists).
  pub path: Option<String>,
  /// The green version the attachment pins (§4.16), or none for a plain or work volume.
  pub version: Option<u64>,
  /// What the daemon established for the requested form (§4.4).
  pub established: Established,
  /// The transport's report for this attachment: the six facts of §4.6 A-9.
  pub capability: AttachmentCapability,
  /// The mount capability token, for an attachment that establishes a host mount (§4.6, §4.13; AUD-01):
  /// the secret the client presents at the NFS mount so the loopback edge authorizes it. `None` for an
  /// attachment that establishes no host mount (an SDK record form, a green pin).
  pub token: Option<[u8; 16]>,
}

/// The result of an `advance` (§4.16 "Attachments and versions"): the version the attachment now
/// pins and the paths the move invalidated — exactly those some version in the span changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advanced {
  /// The version now pinned.
  pub version: u64,
  /// The invalidated paths, sorted.
  pub invalidated: Vec<String>,
}

/// The client.
pub struct Client {
  instance: String,
  end: ClientEnd,
  client_id: u32,
  /// The last sequence used.
  sequence: u32,
  deadlines: Deadlines,
  /// Reconnects made (a non-vacuity counter for the tests of the restart path).
  reconnects: u64,
  /// The highest sequence acknowledged (its completion records released, §4.9).
  acknowledged: u32,
  /// Derived: how many replies the client receives before it acknowledges them: half the
  /// ring's slots, so the daemon retains at most one ring of records per client while the
  /// acknowledgement costs one request in that many.
  ack_every: u32,
  /// The sequences answered `Unpublished` and not yet retried (AC-2.3; AUD-06): the daemon rolled
  /// those verbs back and asked for a retry under the same id, so the acknowledgement watermark
  /// stops below the lowest of them — an acknowledged sequence's record is released, and its retry
  /// would be refused `DuplicateRequest` (the CI Linux lane's failure: a small ring acknowledged
  /// between the refusal and the retry). A retry answered anything else releases the id. Numeric
  /// order: sequences wrap after 2^32 requests.
  unpublished: std::collections::BTreeSet<u32>,
  /// Derived: the bound on unpublished ids kept retryable — the ring's slots (`region.slots`), the
  /// client's own in-flight bound; past it the oldest is forgotten and counted, a caller that never
  /// retries having chosen so.
  unpublished_cap: usize,
  /// Unpublished ids forgotten at the bound.
  unpublished_forgotten: u64,
  /// Replies drained from the completion ring while looking for another request's reply, held by
  /// their request-id word until their own [`Client::poll_reply`] takes them. The async path drains
  /// the ring on a completion-fd signal and matches by id, so replies that arrive out of order (a
  /// deferred verb) do not block another request's. Only replies to `awaited` operations are kept, so
  /// it never holds more than `awaited` does and nothing is ever evicted (AUD-29-22).
  pending: Vec<(u64, ReplyBody)>,
  /// The caller-owned operations outstanding: begun with [`Client::begin`], their replies not yet taken
  /// ([`Client::poll_reply`]) or abandoned ([`Client::abandon`]). Admission refuses a new one at
  /// `awaited_cap` (AUD-29-22): until 2026-10-01 the reply buffer instead evicted its oldest entry past twice
  /// the ring's slots, which a caller that kept old ids while admitting and draining new ones could make an
  /// awaited reply.
  awaited: std::collections::BTreeMap<u64, RequestBody>,
  /// Async edits too large for one request, by the public word their caller awaits: each runs as a chain of
  /// steps (`StageBegin`, `StagePut`s, `EditStaged`), one in flight at a time (bounded by `awaited_cap`, as each
  /// step is awaited).
  edit_chains: std::collections::BTreeMap<u64, EditChain>,
  /// The word of each chain step in flight, mapped to its chain's public word: a step's reply is filed under the
  /// public word, so the caller's event loop sees one operation.
  chain_steps: std::collections::BTreeMap<u64, u64>,
  /// Derived: the outstanding operations admitted — the command ring's slots, the client's in-flight
  /// bound (the same bound the retryable set keeps).
  awaited_cap: usize,
  /// Replies dropped on arrival because nothing awaited them: a protocol-only acknowledgement's, or an
  /// abandoned operation's.
  unawaited_dropped: u64,
  /// The async path's bind of a reconnected channel (AUD-29-19): the attest's request word while it is in
  /// flight, its reply handled where replies are drained, so a reconnect never blocks an event loop on a
  /// round trip.
  attest_in_flight: Option<u64>,
  /// The consumer's refusal of that bind (revoked while the daemon was away): final, every later send
  /// refused with it.
  attest_refused: Option<slates_ipc::protocol::Refusal>,
  /// The consumer this channel is bound to (§4.13) — taken from the harness's delivery at connect, or
  /// attested by the caller — kept so the client binds again on its own after a daemon restart: a
  /// reconnected channel is the account's until it attests, and a retried verb must never run as the
  /// account.
  consumer: Option<Delivered>,
  /// Whether the current channel is bound to `consumer` (false right after a reconnect).
  bound: bool,
  /// Bindings made again after a reconnect (a non-vacuity counter for the restart tests).
  rebinds: u64,
  /// The reconnect claim in flight (AUD-29-19): made by one [`Client::try_reconnect`], its answer read by
  /// the next, so no attempt waits on the daemon.
  reconnecting: Option<Claim>,
}

fn ack_every_of(end: &ClientEnd) -> u32 {
  derived!(
    u32::try_from(end.region().cmd().slots() / 2)
      .unwrap_or(u32::MAX)
      .max(1),
    "slots / 2",
    ["region.slots"]
  )
  .get()
}

impl std::fmt::Debug for Client {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Client")
      .field("instance", &self.instance)
      .field("client_id", &self.client_id)
      .field("sequence", &self.sequence)
      .field("reconnects", &self.reconnects)
      .field("consumer", &self.consumer.as_ref().map(|d| d.consumer))
      .field("bound", &self.bound)
      .finish()
  }
}

/// Derived: the first pause between reconnect attempts is the wake latency the daemon
/// published as its spin window (the shortest wait that is not a spin); each pause doubles
/// up to a tenth of the budget, so a restart is seen within a tenth of the budget at worst
/// and a dead daemon costs no core.
pub(crate) fn next_pause_ns(pause_ns: u64, budget_ns: u64) -> u64 {
  derived!(
    pause_ns.saturating_mul(2).min(budget_ns / 10).max(1),
    "min(2 × pause, reconnect_ns / 10)",
    ["pause_ns", "reconnect_ns"]
  )
  .get()
}

fn elapsed_ns(since: Instant) -> u64 {
  u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Where a chained async edit is ([`Client::edit_begin`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainStep {
  /// `StageBegin` in flight.
  Begin,
  /// A `StagePut` in flight.
  Put,
  /// The `EditStaged` in flight.
  Commit,
}

/// An async edit too large for one request, run as a chain of requests ([`Client::edit_begin`]).
#[derive(Debug)]
struct EditChain {
  work: VolumeId,
  path: String,
  at: u64,
  delete_len: u64,
  bytes: Vec<u8>,
  step: ChainStep,
  token: u64,
  /// How many of `bytes` are put (or in flight).
  offset: usize,
}

/// Maps a decoded reply body to a result: a refusal becomes the typed error, any other body passes
/// through — the same rule [`Client::exchange`] applies to a synchronous reply.
fn resolved(body: ReplyBody) -> Result<ReplyBody, ClientError> {
  match body {
    ReplyBody::Refused { refusal } => Err(ClientError::Refused(refusal)),
    other => Ok(other),
  }
}

/// Extracts a created volume's id from its reply, or a typed mismatch — the async typed `create`
/// verb's leg, sharing the sync `create`'s rule.
fn extract_created(body: ReplyBody) -> Result<VolumeId, ClientError> {
  match body {
    ReplyBody::Created { id } => Ok(id),
    _ => Err(ClientError::UnexpectedReply { verb: "create" }),
  }
}

/// Extracts a snapshot's id from its reply, or a typed mismatch.
fn extract_snapshotted(body: ReplyBody) -> Result<SnapshotId, ClientError> {
  match body {
    ReplyBody::Snapshotted { id, .. } => Ok(id),
    _ => Err(ClientError::UnexpectedReply { verb: "snapshot" }),
  }
}

/// Extracts a status report from its reply, or a typed mismatch.
fn extract_status(body: ReplyBody) -> Result<StatusReport, ClientError> {
  match body {
    ReplyBody::Status { report } => Ok(*report),
    _ => Err(ClientError::UnexpectedReply { verb: "status" }),
  }
}

/// Extracts the volume list from its reply, or a typed mismatch.
fn extract_listed(body: ReplyBody) -> Result<Vec<VolumeSummary>, ClientError> {
  match body {
    ReplyBody::Listed { volumes } => Ok(volumes),
    _ => Err(ClientError::UnexpectedReply { verb: "list" }),
  }
}

/// Confirms a resize's reply, or a typed mismatch. A unit verb yields `true` (not the `None` a poll
/// returns when the reply has not come), so an async pump can tell "done" from "not yet".
fn extract_resized(body: ReplyBody) -> Result<bool, ClientError> {
  match body {
    ReplyBody::Resized => Ok(true),
    _ => Err(ClientError::UnexpectedReply { verb: "resize" }),
  }
}

/// Confirms a destroy's reply, or a typed mismatch (yields `true`, as [`extract_resized`] does).
fn extract_destroyed(body: ReplyBody) -> Result<bool, ClientError> {
  match body {
    ReplyBody::Destroyed => Ok(true),
    _ => Err(ClientError::UnexpectedReply { verb: "destroy" }),
  }
}

/// Extracts a created green's id from its reply, or a typed mismatch.
fn extract_green_created(body: ReplyBody) -> Result<VolumeId, ClientError> {
  match body {
    ReplyBody::GreenCreated { id } => Ok(id),
    _ => Err(ClientError::UnexpectedReply {
      verb: "create_green",
    }),
  }
}

/// Extracts a created work's id and base version from its reply, or a typed mismatch.
fn extract_work_created(body: ReplyBody) -> Result<(VolumeId, u64), ClientError> {
  match body {
    ReplyBody::WorkCreated { id, base } => Ok((id, base)),
    _ => Err(ClientError::UnexpectedReply {
      verb: "create_work",
    }),
  }
}

/// Confirms an edit's reply, or a typed mismatch (yields `true`, the unit-verb sentinel).
fn extract_edited(body: ReplyBody) -> Result<bool, ClientError> {
  match body {
    ReplyBody::Edited => Ok(true),
    _ => Err(ClientError::UnexpectedReply { verb: "edit" }),
  }
}

/// Extracts an advance's outcome (the pinned version and the invalidated paths), or a typed mismatch.
fn extract_advanced(body: ReplyBody) -> Result<Advanced, ClientError> {
  match body {
    ReplyBody::Advanced {
      version,
      invalidated,
    } => Ok(Advanced {
      version,
      invalidated,
    }),
    _ => Err(ClientError::UnexpectedReply { verb: "advance" }),
  }
}

/// Extracts a read's bytes, or a typed mismatch.
fn extract_read(body: ReplyBody) -> Result<Vec<u8>, ClientError> {
  match body {
    ReplyBody::ReadBytes { bytes } => Ok(bytes),
    _ => Err(ClientError::UnexpectedReply { verb: "read" }),
  }
}

/// Extracts a submit's outcome from its reply — accepted at a version, or a conflict with windows —
/// or a typed mismatch. The same rule the sync [`Client::submit`] applies.
fn extract_submitted(body: ReplyBody) -> Result<Submitted, ClientError> {
  match body {
    ReplyBody::Submitted {
      version: Some(v), ..
    } => Ok(Submitted::Accepted(v)),
    ReplyBody::Submitted {
      version: None,
      conflicts,
    } => Ok(Submitted::Conflict(conflicts)),
    _ => Err(ClientError::UnexpectedReply { verb: "submit" }),
  }
}

/// Extracts a green's head version from its reply, or a typed mismatch.
fn extract_versions(body: ReplyBody) -> Result<u64, ClientError> {
  match body {
    ReplyBody::Versions { head } => Ok(head),
    _ => Err(ClientError::UnexpectedReply { verb: "versions" }),
  }
}

/// Extracts the changed-paths list from its reply, or a typed mismatch.
fn extract_changed(body: ReplyBody) -> Result<Vec<String>, ClientError> {
  match body {
    ReplyBody::ChangedSince { paths } => Ok(paths),
    _ => Err(ClientError::UnexpectedReply {
      verb: "changed_since",
    }),
  }
}

/// Extracts a rebase's outcome — rebased onto a version, or a conflict — or a typed mismatch.
fn extract_rebased(body: ReplyBody) -> Result<Rebased, ClientError> {
  match body {
    ReplyBody::Rebased {
      version: Some(v), ..
    } => Ok(Rebased::Rebased(v)),
    ReplyBody::Rebased {
      version: None,
      conflicts,
    } => Ok(Rebased::Conflict(conflicts)),
    _ => Err(ClientError::UnexpectedReply { verb: "rebase" }),
  }
}

/// Confirms a namespace declaration's reply, or a typed mismatch (yields `true`, the unit sentinel).
fn extract_declared(body: ReplyBody) -> Result<bool, ClientError> {
  match body {
    ReplyBody::Declared => Ok(true),
    _ => Err(ClientError::UnexpectedReply { verb: "declare" }),
  }
}

/// Extracts a landing's outcome from its reply — landed, or a grant required — or a typed mismatch.
/// The same rule the sync [`Client::land`] applies.
fn extract_landed(body: ReplyBody) -> Result<Landing, ClientError> {
  match body {
    ReplyBody::Landed { outcome } => Ok(Landing::Landed(outcome)),
    ReplyBody::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    } => Ok(Landing::GrantRequired {
      landing,
      manifest,
      summary,
      conflicts,
    }),
    _ => Err(ClientError::UnexpectedReply { verb: "land" }),
  }
}

/// The capability the harness delivered to this process, if it was spawned as a consumer (§4.13;
/// `slates_ipc::delivery`): none when the delivery variable is absent — the process is the account's
/// own client — and a typed refusal when it is present but unusable, so a broken delivery never
/// degrades into the account's ambient authority.
/// A connect in flight for an event loop ([`Client::begin_connect`]; AUD-29-19): the rendezvous claim
/// made, its answer read by [`poll`](Self::poll) whenever the loop looks, so a slow, stopped or dead
/// daemon never holds the loop. Dropped unanswered, the claim is given back.
pub struct Connecting {
  instance: String,
  deadlines: Deadlines,
  delivered: Option<Delivered>,
  /// The claim; `None` once the connect has ended (connected or refused).
  claim: Option<Claim>,
  /// The pacing of the loop's next look, from [`next_poll_ns`](Self::next_poll_ns).
  pause_ns: u64,
}

impl std::fmt::Debug for Connecting {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Connecting")
      .field("instance", &self.instance)
      .finish()
  }
}

impl Connecting {
  /// Reads the claim's answer without waiting: `Some` the connected client, `None` while it is due, a
  /// typed refusal when the daemon refused the claim or left it unanswered past the claim wait. A poll
  /// after the connect ended is refused.
  pub fn poll(&mut self) -> Result<Option<Client>, ClientError> {
    let Some(claim) = self.claim.as_mut() else {
      return Err(ClientError::Ipc(IpcError::DaemonUnavailable {
        endpoint: self.instance.clone(),
        why: "the connect has already ended",
      }));
    };
    match claim.poll() {
      Ok(None) => Ok(None),
      Ok(Some(connected)) => {
        self.claim = None;
        let mut client = Client::over(&self.instance, connected, 0, self.deadlines);
        // Bound on the first send, without waiting (`bind_without_waiting`).
        client.consumer = self.delivered.take();
        Ok(Some(client))
      }
      Err(error) => {
        self.claim = None;
        Err(ClientError::Ipc(error))
      }
    }
  }

  /// When the loop should look again, in nanoseconds from now: paced as a reconnect is (doubling, at most
  /// a tenth of the claim wait), so a prompt answer is read within microseconds and a slow one costs at
  /// most ten looks per claim wait.
  pub fn next_poll_ns(&mut self) -> u64 {
    self.pause_ns = next_pause_ns(self.pause_ns, CLAIM_WAIT_NS);
    self.pause_ns
  }
}

fn delivered_capability() -> Result<Option<Delivered>, ClientError> {
  match slates_ipc::delivery::delivered() {
    Ok(delivered) => Ok(Some(delivered.clone())),
    Err(IpcError::CapabilityNotDelivered {
      fault: DeliveryFault::Absent,
    }) => Ok(None),
    Err(e) => Err(ClientError::Ipc(e)),
  }
}

impl Client {
  /// Connects to `instance` as a new client. A process spawned as a consumer — its capability
  /// delivered on the inherited descriptor (§4.13, `slates_ipc::delivery`) — binds the channel to
  /// that consumer before returning, so nothing runs on it as the account; a process without a
  /// delivery is the account's client.
  pub fn connect(instance: &str, deadlines: Deadlines) -> Result<Client, ClientError> {
    let delivered = delivered_capability()?;
    let connected = connect_as(instance, 0, deadlines.reconnect_ns)?;
    let mut client = Client::over(instance, connected, 0, deadlines);
    client.bind_delivered(delivered)?;
    Ok(client)
  }

  /// Starts a connect for an event loop (AUD-29-19): the claim is made and nothing waits;
  /// [`Connecting::poll`] reads its answer. A delivered capability binds the channel on its first send,
  /// without waiting, as a reconnected channel binds again.
  pub fn begin_connect(instance: &str, deadlines: Deadlines) -> Result<Connecting, ClientError> {
    let delivered = delivered_capability()?;
    let claim = begin_connect_as(instance, 0, deadlines.reconnect_ns)?;
    Ok(Connecting {
      instance: instance.to_owned(),
      deadlines,
      delivered,
      claim: Some(claim),
      pause_ns: 0,
    })
  }

  /// Resumes a session: connects under its client id (refused `SessionTaken` when a live
  /// client holds it) and continues its sequence, so a retry of what it had in flight meets
  /// the completion record (§4.9). A delivered capability binds the resumed channel as
  /// [`Client::connect`] does — a restarted workload is spawned with its own delivery.
  pub fn resume(
    instance: &str,
    session: Session,
    deadlines: Deadlines,
  ) -> Result<Client, ClientError> {
    let delivered = delivered_capability()?;
    let connected = connect_as(instance, session.client_id, deadlines.reconnect_ns)?;
    let assigned = connected.region.client_id();
    if assigned != session.client_id {
      return Err(ClientError::SessionTaken { assigned });
    }
    let mut client = Client::over(
      instance,
      connected,
      session.next_sequence.saturating_sub(1),
      deadlines,
    );
    client.bind_delivered(delivered)?;
    Ok(client)
  }

  /// A client over a fresh rendezvous, its sequence at `sequence` (the last used).
  fn over(instance: &str, connected: Connected, sequence: u32, deadlines: Deadlines) -> Client {
    let client_id = connected.region.client_id();
    let end = ClientEnd::connected(connected);
    let ack_every = ack_every_of(&end);
    let unpublished_cap = end.region().cmd().slots().max(1);
    Client {
      instance: instance.to_owned(),
      end,
      client_id,
      sequence,
      deadlines,
      reconnects: 0,
      acknowledged: 0,
      ack_every,
      unpublished: std::collections::BTreeSet::new(),
      unpublished_cap,
      unpublished_forgotten: 0,
      pending: Vec::new(),
      awaited: std::collections::BTreeMap::new(),
      edit_chains: std::collections::BTreeMap::new(),
      chain_steps: std::collections::BTreeMap::new(),
      awaited_cap: unpublished_cap,
      unawaited_dropped: 0,
      attest_in_flight: None,
      attest_refused: None,
      consumer: None,
      bound: false,
      rebinds: 0,
      reconnecting: None,
    }
  }

  /// Binds the channel to the delivered consumer, when there is one.
  fn bind_delivered(&mut self, delivered: Option<Delivered>) -> Result<(), ClientError> {
    match delivered {
      Some(delivered) => self.attest(delivered.consumer, &delivered.capability),
      None => Ok(()),
    }
  }

  /// Binds this channel to `consumer` with its capability (§4.13 `Attest`): the proof is keyed over
  /// this channel's client id, so nothing captured from another session binds it. On success the
  /// channel's principal is the consumer for every later verb — and, holding the capability, the
  /// client binds again by itself after a daemon restart, before any retried verb runs. Refused
  /// `ConsumerNotEnrolled` (no such consumer, or the capability is wrong) or `ConsumerRevoked`.
  pub fn attest(&mut self, consumer: u64, capability: &Capability) -> Result<(), ClientError> {
    let proof = attest_proof(capability, self.client_id);
    match self.call(&RequestBody::Attest { consumer, proof })? {
      ReplyBody::Attested => {
        self.consumer = Some(Delivered {
          consumer,
          capability: *capability,
        });
        self.bound = true;
        Ok(())
      }
      _ => Err(ClientError::UnexpectedReply { verb: "attest" }),
    }
  }

  /// The consumer this channel is bound to, if any (§4.13); the account's client has none.
  pub fn consumer(&self) -> Option<u64> {
    self.consumer.as_ref().map(|delivered| delivered.consumer)
  }

  /// Bindings made again after a reconnect so far (the non-vacuity counter of the restart path).
  pub fn rebinds(&self) -> u64 {
    self.rebinds
  }

  /// The client id the daemon bound to this principal.
  pub fn client_id(&self) -> u32 {
    self.client_id
  }

  /// The session to resume later.
  pub fn session(&self) -> Session {
    Session {
      client_id: self.client_id,
      // At most `LAST_SEQUENCE + 1 = u32::MAX`, the exhausted marker: exact, never a wrap.
      next_sequence: self.sequence.saturating_add(1),
    }
  }

  /// The last request id used (what a retry after a restart would carry).
  pub fn last_request(&self) -> RequestId {
    RequestId {
      client: self.client_id,
      sequence: self.sequence,
    }
  }

  /// Spins this long for a reply before parking (`None`: the client's wake estimate — seeded with
  /// the daemon's published window, the measured wake cost, and refined from the client's own parks;
  /// the 2-competitive choice for CPU). A caller with a latency floor of its own spins for it and
  /// never pays a wake when the daemon meets it.
  pub fn spin_for(&mut self, spin_ns: Option<u64>) {
    self.end.set_spin_ns(spin_ns);
  }

  /// Reconnects made so far.
  pub fn reconnects(&self) -> u64 {
    self.reconnects
  }

  /// Parks and replies so far (the measured spin-to-park ratio).
  pub fn park_ratio(&self) -> (u64, u64) {
    self.end.park_ratio()
  }

  /// Parks a wake ended with no reply waiting, so the client parked again.
  pub fn unanswered_wakes(&self) -> u64 {
    self.end.unanswered_wakes()
  }

  /// Sends `body` as the next request and returns the reply body, refusals typed.
  pub fn call(&mut self, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    // Every `ack_every` replies, the client acknowledges them first (one request), so the
    // daemon's retained records stay bounded without the caller's help (§4.9) — up to the
    // watermark, which an unpublished id not yet retried holds back.
    let up_to = self.ack_watermark();
    if up_to.saturating_sub(self.acknowledged) >= self.ack_every {
      self.acknowledge(up_to)?;
    }
    if matches!(body, RequestBody::DaemonStatus) {
      let capacity = slates_ipc::status::snapshot_capacity(self.end.region());
      return slates_ipc::status::collect(capacity, |request| self.call_plain(request));
    }
    self.call_plain(body)
  }

  /// The highest sequence the client may acknowledge now: everything it has received, unless a verb
  /// answered `Unpublished` awaits its retry — then the sequence just below the lowest such id, so
  /// the daemon keeps the ids above it new and the retry re-executes rather than meeting
  /// `DuplicateRequest` (AC-2.3; AUD-06).
  fn ack_watermark(&self) -> u32 {
    self
      .unpublished
      .first()
      .map_or(self.sequence, |&lowest| lowest.saturating_sub(1))
  }

  /// Notes what `word`'s reply means for the retryable set: an `Unpublished` refusal keeps the id
  /// retryable (bounded, the oldest forgotten and counted past the ring's slots); any other answer
  /// to a retried id releases it.
  fn note_reply(&mut self, word: u64, body: &ReplyBody) {
    let sequence = RequestId::from_word(word).sequence;
    if matches!(
      body,
      ReplyBody::Refused {
        refusal: slates_ipc::protocol::Refusal::Unpublished { .. }
      }
    ) {
      if self.unpublished.len() >= self.unpublished_cap
        && let Some(oldest) = self.unpublished.pop_first()
      {
        self.unpublished_forgotten = self.unpublished_forgotten.saturating_add(1);
        let _ = oldest;
      }
      self.unpublished.insert(sequence);
    } else {
      self.unpublished.remove(&sequence);
    }
  }

  /// Unpublished ids forgotten at the retryable bound (a caller that never retried them).
  pub fn unpublished_forgotten(&self) -> u64 {
    self.unpublished_forgotten
  }

  /// One request under the next sequence, without the automatic acknowledgement.
  fn call_plain(&mut self, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    let id = self.fresh_id()?;
    self.exchange(id, body)
  }

  /// Sends `body` under `id` again (a retry: the daemon answers from its completion record
  /// when it served the id before, else serves it now).
  pub fn retry(&mut self, id: RequestId, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    self.exchange(id, body)
  }

  /// One request, one reply; a stalled reply from a gone daemon becomes a reconnect — the fresh
  /// channel bound again to the consumer first, when the client holds one — and a resend under the
  /// same id.
  fn exchange(&mut self, id: RequestId, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    loop {
      self.rebind_if_needed()?;
      if let Some(reply) = self.round_trip(id, body)? {
        self.note_reply(id.word(), &reply);
        return resolved(reply);
      }
    }
  }

  /// The binding a reconnected channel still needs: the consumer and the proof for this channel.
  fn pending_bind(&self) -> Option<(u64, [u8; 32])> {
    if self.bound {
      return None;
    }
    self.consumer.as_ref().map(|delivered| {
      (
        delivered.consumer,
        attest_proof(&delivered.capability, self.client_id),
      )
    })
  }

  /// After a reconnect the fresh channel's principal is the account's; a client holding a consumer
  /// identity binds it again before any verb runs on the channel, so a retried verb never runs as
  /// the account (§4.13). A refusal — the consumer revoked while the daemon was away — is final: the
  /// verb is not sent.
  fn rebind_if_needed(&mut self) -> Result<(), ClientError> {
    while let Some((consumer, proof)) = self.pending_bind() {
      let id = self.fresh_id()?;
      match self.round_trip(id, &RequestBody::Attest { consumer, proof })? {
        // The daemon went away again during the bind and the client reconnected: bind the newer channel.
        None => {}
        Some(ReplyBody::Attested) => {
          self.bound = true;
          self.rebinds = self.rebinds.saturating_add(1);
        }
        Some(ReplyBody::Refused { refusal }) => return Err(ClientError::Refused(refusal)),
        Some(_) => return Err(ClientError::UnexpectedReply { verb: "attest" }),
      }
    }
    Ok(())
  }

  /// The next request id, or `SequencesExhausted` past [`LAST_SEQUENCE`] — refused before anything is sent,
  /// never a wrap into an id the daemon has already seen (AUD-29-21).
  fn fresh_id(&mut self) -> Result<RequestId, ClientError> {
    let id = self.next_id()?;
    self.sequence = id.sequence;
    Ok(id)
  }

  /// The next request id without using it (a send that does not happen leaves the sequence unused), or
  /// `SequencesExhausted` past [`LAST_SEQUENCE`].
  fn next_id(&self) -> Result<RequestId, ClientError> {
    if self.sequence >= LAST_SEQUENCE {
      return Err(ClientError::SequencesExhausted {
        client: self.client_id,
      });
    }
    Ok(RequestId {
      client: self.client_id,
      sequence: self.sequence.saturating_add(1),
    })
  }

  /// One send and one wait for `id`'s reply: the decoded reply, or `None` when the daemon was found
  /// gone and the client reconnected instead (the caller binds and resends).
  fn round_trip(
    &mut self,
    id: RequestId,
    body: &RequestBody,
  ) -> Result<Option<ReplyBody>, ClientError> {
    if !self.send(id, body)? {
      return Ok(None);
    }
    loop {
      match self.end.wait(Some(self.deadlines.reply_ns)) {
        Ok(reply) => {
          if reply.request != id.word() {
            // Another request's reply (an async call's, or an abandoned one's still in flight): kept for its
            // caller when one awaits it, else dropped and counted, as the async drain does (AUD-29-22); this wait
            // goes on. Bounded: no more replies are in flight than the ring has slots.
            let body: ReplyBody = unpack(self.end.region(), reply.kind, &reply.payload)?;
            self.buffer(reply.request, body);
            continue;
          }
          return Ok(Some(unpack(self.end.region(), reply.kind, &reply.payload)?));
        }
        Err(IpcError::DeadlineExceeded) if self.end.daemon_gone() => {
          self.reconnect()?;
          return Ok(None);
        }
        // A verb whose reply the daemon defers until long work ends (a granted landing) is waited for
        // while the daemon lives: its supervisor ends a daemon that stops answering, and the wait then ends
        // as `DaemonGone` and a resend, never as a stall of work still running (AUD-29-25's sibling).
        Err(IpcError::DeadlineExceeded) if defers_reply(body) => {}
        Err(IpcError::DeadlineExceeded) => {
          return Err(ClientError::Stalled {
            after_ns: self.deadlines.reply_ns,
          });
        }
        Err(e) => return Err(ClientError::Ipc(e)),
      }
    }
  }

  // --- The async round trip (§4.7 "Wake strategy", R6, D-19) --------------------------------
  //
  // The sync `call` blocks on `end.wait` (spin then park). The async form splits that in two so a
  // host event loop drives the wait: `begin` sends and returns the request id; the caller spins
  // with `spin_reply` (the fast path, no event loop) and, if the reply has not landed, arms the
  // completion signal, yields to its loop on the completion fd, and takes the reply with
  // `poll_reply` when the fd signals. The SDK bindings own the yield on their own loop
  // (`asyncio.add_reader`, `uv_poll`); this core provides the non-blocking primitives and matches
  // replies to requests by id, so one reader can serve every request in flight.

  /// Sends `body` under the next sequence and returns its request id, without waiting for the reply
  /// — the async caller awaits it on the completion fd. Unlike [`Self::call`], this sends no periodic
  /// acknowledgement inline (a blocking round trip has no place on the async path); an async caller
  /// acknowledges by a sync [`Self::acknowledge`] between calls, or by `begin`-ing the ack body and
  /// dropping its reply.
  pub fn begin(&mut self, body: &RequestBody) -> Result<RequestId, ClientError> {
    if self.awaited.len() >= self.awaited_cap {
      return Err(ClientError::TooManyOutstanding {
        limit: self.awaited_cap,
      });
    }
    let id = self.send_once(body)?;
    // The body is kept while the call is outstanding (bounded by admission), so a lost channel's calls are
    // resent under their own ids ([`Self::resend_awaited`]).
    self.awaited.insert(id.word(), body.clone());
    Ok(id)
  }

  /// Sends `body` under the next sequence, owned by no caller: a protocol-only request whose reply is
  /// dropped on arrival (the periodic acknowledgement).
  fn begin_unawaited(&mut self, body: &RequestBody) -> Result<RequestId, ClientError> {
    self.send_once(body)
  }

  /// One attempt to send `body` under the next sequence, never waiting (AUD-29-19): the id when it went;
  /// `RingFull` when the ring has no slot, `ChannelLost` when the daemon is gone — either way nothing was
  /// sent and the sequence is not used. A channel just reconnected is bound to the consumer first (one
  /// round trip on the fresh channel, bounded by the reply deadline), so no request runs as the account.
  fn send_once(&mut self, body: &RequestBody) -> Result<RequestId, ClientError> {
    self.bind_without_waiting()?;
    let id = self.next_id()?;
    if self.try_send(id, body)? {
      self.sequence = id.sequence;
      Ok(id)
    } else if self.end.daemon_gone() {
      Err(ClientError::ChannelLost)
    } else {
      Err(ClientError::RingFull)
    }
  }

  /// Sends `body` again under `word` — an outstanding operation's own id, so the daemon answers from its
  /// completion record if it served it before the channel was lost, and serves it now if not (AUD-29-20).
  /// One attempt: `Ok(false)` when the ring has no slot.
  pub fn resend(&mut self, word: u64, body: &RequestBody) -> Result<bool, ClientError> {
    self.bind_without_waiting()?;
    self.try_send(RequestId::from_word(word), body)
  }

  /// [`Self::resend`] for an outstanding call, with the body it was begun with; `Ok(true)` too for a word no
  /// longer outstanding (nothing is owed for it).
  pub fn resend_awaited(&mut self, word: u64) -> Result<bool, ClientError> {
    let Some(body) = self.awaited.get(&word).cloned() else {
      return Ok(true);
    };
    self.resend(word, &body)
  }

  /// The async path's bind of a reconnected channel (AUD-29-19): nothing to do when the channel is bound
  /// or the client holds no consumer; otherwise the attest goes out (once) and every send is refused
  /// `Rebinding` until its reply is drained — never a blocking round trip on the caller's thread. A refused
  /// bind is final.
  fn bind_without_waiting(&mut self) -> Result<(), ClientError> {
    if let Some(refusal) = &self.attest_refused {
      return Err(ClientError::Refused(refusal.clone()));
    }
    let Some((consumer, proof)) = self.pending_bind() else {
      return Ok(());
    };
    if self.attest_in_flight.is_none() {
      let id = self.next_id()?;
      if !self.try_send(id, &RequestBody::Attest { consumer, proof })? {
        return Err(if self.end.daemon_gone() {
          ClientError::ChannelLost
        } else {
          ClientError::RingFull
        });
      }
      self.sequence = id.sequence;
      self.attest_in_flight = Some(id.word());
    }
    Err(ClientError::Rebinding)
  }

  /// Whether the daemon is gone from this channel (its liveness signal; a cold-path question, asked when a
  /// reply is overdue).
  pub fn daemon_gone(&self) -> bool {
    self.end.daemon_gone()
  }

  /// One step of a reconnect under the client's id, never waiting (AUD-29-19): the first step makes the
  /// claim, each later one reads its answer. `Ok(true)` connected (the fresh channel is bound to the
  /// consumer on the next send); `Ok(false)` while the answer is due, or when no daemon answered (the
  /// next step claims again). A live client holding the id is refused `SessionTaken`.
  pub fn try_reconnect(&mut self) -> Result<bool, ClientError> {
    let polled = match self.reconnecting.as_mut() {
      Some(claim) => claim.poll(),
      None => match begin_connect_as(&self.instance, self.client_id, self.deadlines.reconnect_ns) {
        Ok(claim) => self.reconnecting.insert(claim).poll(),
        Err(error) => Err(error),
      },
    };
    if !matches!(polled, Ok(None)) {
      self.reconnecting = None;
    }
    match polled {
      Ok(None) => Ok(false),
      Ok(Some(connected)) => {
        let assigned = connected.region.client_id();
        if assigned != self.client_id {
          return Err(ClientError::SessionTaken { assigned });
        }
        self.end = ClientEnd::connected(connected);
        self.reconnects = self.reconnects.saturating_add(1);
        self.bound = false;
        self.attest_in_flight = None;
        Ok(true)
      }
      Err(IpcError::DaemonUnavailable { .. } | IpcError::RingFull) => Ok(false),
      Err(e) => Err(ClientError::Ipc(e)),
    }
  }

  /// The client's derived budgets.
  pub fn deadlines(&self) -> Deadlines {
    self.deadlines
  }

  /// The caller-owned operations the client admits at once ([`ClientError::TooManyOutstanding`]).
  pub fn outstanding_limit(&self) -> usize {
    self.awaited_cap
  }

  /// Releases an outstanding operation the caller no longer awaits (a cancelled call, a call settled by a
  /// terminal channel failure): its buffered reply, if any, is dropped, and a reply that arrives later is
  /// dropped on arrival. Returns whether it was outstanding.
  pub fn abandon(&mut self, word: u64) -> bool {
    self.pending.retain(|(held, _)| *held != word);
    if self.edit_chains.remove(&word).is_some() {
      let steps: Vec<u64> = self
        .chain_steps
        .iter()
        .filter(|(_, public)| **public == word)
        .map(|(step, _)| *step)
        .collect();
      for step in steps {
        self.chain_steps.remove(&step);
        self.awaited.remove(&step);
      }
      // The chain's first step travels under the public word itself.
      self.awaited.remove(&word);
      return true;
    }
    self.awaited.remove(&word).is_some()
  }

  /// The caller-owned operations outstanding, by id word — what a binding settles when its channel ends. A chained
  /// edit is one operation, under its public word.
  pub fn outstanding(&self) -> Vec<u64> {
    self
      .awaited
      .keys()
      .filter(|word| !self.chain_steps.contains_key(word))
      .copied()
      .chain(self.edit_chains.keys().copied())
      .collect::<std::collections::BTreeSet<u64>>()
      .into_iter()
      .collect()
  }

  /// Replies dropped on arrival because nothing awaited them (a protocol-only acknowledgement's, or an
  /// abandoned operation's) — the counter a test reads to see the drop path ran.
  pub fn unawaited_dropped(&self) -> u64 {
    self.unawaited_dropped
  }

  /// Takes the reply to `id` if it has arrived, without blocking. Replies for other requests drained
  /// while looking are buffered by id for their own `poll_reply`; a refusal is returned typed. `None`
  /// until `id`'s reply is on the ring.
  pub fn poll_reply(&mut self, id: RequestId) -> Result<Option<ReplyBody>, ClientError> {
    self.poll_reply_word(id.word())
  }

  /// [`Self::poll_reply`] by the request id's word — the form an async pump uses, which holds a
  /// request's id as its `u64` word across the yield to its event loop.
  pub fn poll_reply_word(&mut self, word: u64) -> Result<Option<ReplyBody>, ClientError> {
    if let Some(pos) = self.pending.iter().position(|(held, _)| *held == word) {
      let (_, body) = self.pending.remove(pos);
      self.awaited.remove(&word);
      self.note_reply(word, &body);
      return resolved(body).map(Some);
    }
    while let Some(reply) = self.end.try_take()? {
      let body: ReplyBody = unpack(self.end.region(), reply.kind, &reply.payload)?;
      if reply.request == word {
        self.awaited.remove(&word);
        self.note_reply(word, &body);
        return resolved(body).map(Some);
      }
      self.buffer(reply.request, body);
    }
    Ok(None)
  }

  /// Spins for `spin_ns` taking `id`'s reply if it lands — the async fast path: most replies arrive
  /// within the daemon's published spin window and never touch the event loop (§4.7 worked example).
  /// `None` if the reply has not come by the window's end, so the caller yields to its loop.
  pub fn spin_reply(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<ReplyBody>, ClientError> {
    let started = Instant::now();
    loop {
      if let Some(reply) = self.poll_reply(id)? {
        return Ok(Some(reply));
      }
      if elapsed_ns(started) >= spin_ns {
        return Ok(None);
      }
      std::hint::spin_loop();
    }
  }

  /// Drains every reply now on the completion ring into the per-id buffer and returns the ids held,
  /// so an async pump resolves each waiting request in one pass (an event loop allows one reader per
  /// fd, so one reader serves every request in flight). Non-blocking.
  pub fn take_ready(&mut self) -> Result<Vec<u64>, ClientError> {
    while let Some(reply) = self.end.try_take()? {
      let body: ReplyBody = unpack(self.end.region(), reply.kind, &reply.payload)?;
      self.buffer(reply.request, body);
    }
    Ok(self.pending.iter().map(|(word, _)| *word).collect())
  }

  /// Buffers a reply drained for another request: kept when an outstanding operation awaits it (bounded by
  /// the admitted outstanding set, so nothing is evicted, item 8), dropped and counted when nothing does —
  /// a protocol-only acknowledgement's reply, or an abandoned operation's (AUD-29-22).
  fn buffer(&mut self, id_word: u64, body: ReplyBody) {
    // A chain step's reply is its chain's: filed under the public word the caller awaits.
    if let Some(public) = self.chain_steps.remove(&id_word) {
      self.awaited.remove(&id_word);
      self.note_reply(id_word, &body);
      if !self.pending.iter().any(|(held, _)| *held == public) {
        self.pending.push((public, body));
      }
      return;
    }
    if self.attest_in_flight == Some(id_word) {
      self.attest_in_flight = None;
      match body {
        ReplyBody::Attested => {
          self.bound = true;
          self.rebinds = self.rebinds.saturating_add(1);
        }
        ReplyBody::Refused { refusal } => self.attest_refused = Some(refusal),
        // Anything else is not this bind's answer: the next send attests again.
        _ => {}
      }
      return;
    }
    if self.awaited.contains_key(&id_word) && !self.pending.iter().any(|(held, _)| *held == id_word)
    {
      self.pending.push((id_word, body));
    } else {
      self.unawaited_dropped = self.unawaited_dropped.saturating_add(1);
    }
  }

  /// Enables the async completion channel and returns the descriptor an event loop polls
  /// (`add_reader` / `uv_poll`, D-19): the SDK registers it, arms with [`Self::arm_async`] before it
  /// yields, and drains it with [`Self::drain_completion`] when the loop reports it readable.
  #[cfg(unix)]
  pub fn enable_async_completion(&mut self) -> Result<RawFd, ClientError> {
    self.end.enable_async_completion().map_err(ClientError::Ipc)
  }

  /// Like [`Self::enable_async_completion`] but returns a dup the caller owns and must close — for an
  /// SDK whose event loop closes the descriptor it polls (Node's `net.Socket`), not one that only
  /// polls it (Python `asyncio`). Closing the dup leaves the client's own fd intact.
  #[cfg(unix)]
  pub fn enable_async_completion_dup(&mut self) -> Result<RawFd, ClientError> {
    self
      .end
      .enable_async_completion_dup()
      .map_err(ClientError::Ipc)
  }

  /// The Windows analogue of [`Self::enable_async_completion`], returning the loopback `SOCKET` an
  /// event loop polls (`uv_poll` / a Python selector, D-19): Windows passes no shared completion fd
  /// (D-10), so this starts the completion bridge on first use and hands back its socket.
  #[cfg(windows)]
  pub fn enable_async_completion(&mut self) -> Result<RawSocket, ClientError> {
    self.end.enable_async_completion().map_err(ClientError::Ipc)
  }

  /// The Windows analogue of [`Self::enable_async_completion_dup`]: a dup of the completion socket the
  /// caller owns and closes (Node's `net.Socket` adopts and closes it), leaving the client's intact.
  #[cfg(windows)]
  pub fn enable_async_completion_dup(&mut self) -> Result<RawSocket, ClientError> {
    self
      .end
      .enable_async_completion_dup()
      .map_err(ClientError::Ipc)
  }

  /// Arms the completion signal before the SDK yields to its event loop (the daemon wakes a parked
  /// client on a reply). A caller re-checks [`Self::poll_reply`] right after arming to close the race
  /// with a reply that landed during the spin.
  pub fn arm_async(&mut self) -> Result<(), ClientError> {
    self.end.arm_async().map_err(ClientError::Ipc)
  }

  /// Clears the arm once a reply is taken, so a later fast-path reply costs the daemon no wake.
  pub fn disarm_async(&mut self) -> Result<(), ClientError> {
    self.end.disarm_async().map_err(ClientError::Ipc)
  }

  /// Clears the completion fd's readiness after the loop reports it readable.
  #[cfg(unix)]
  pub fn drain_completion(&self) {
    self.end.drain_completion();
  }

  /// Clears the completion socket's readiness after the loop reports it readable (Windows).
  #[cfg(windows)]
  pub fn drain_completion(&self) {
    self.end.drain_completion();
  }

  /// Sends the periodic acknowledgement if it is due, without waiting for its reply — the async
  /// caller drains and drops the `Acknowledged` reply as an id it never awaited. This keeps the
  /// daemon's retained completion records bounded (§4.9) the way [`Self::call`] does inline on the
  /// sync path, without a blocking round trip on the event loop. The acknowledged mark advances
  /// optimistically; a lost ack only means the daemon holds a little more until the next one.
  pub fn begin_ack_if_due(&mut self) -> Result<(), ClientError> {
    let up_to = self.ack_watermark();
    if up_to.saturating_sub(self.acknowledged) >= self.ack_every {
      // A full ring or a lost channel defers the acknowledgement to the next call that finds it due.
      match self.begin_unawaited(&RequestBody::Acknowledge { up_to }) {
        Ok(_) => self.acknowledged = self.acknowledged.max(up_to),
        Err(ClientError::RingFull | ClientError::ChannelLost) => {}
        Err(other) => return Err(other),
      }
    }
    Ok(())
  }

  /// The spin window the daemon published (nanoseconds): the async fast path spins this long taking
  /// the reply before it arms and yields to its event loop. The async path does not refine it: its wake
  /// is the event loop's, which the client does not time (the sync wait's estimate is
  /// `ClientEnd::wake_estimate_ns`).
  pub fn published_spin_ns(&self) -> u64 {
    u64::from(self.end.region().spin_ns())
  }

  // Typed async verbs (R6, D-19): a `begin` that sends and returns the id, a `spin` that takes the
  // reply within the spin window (the fast path), and a `poll` that takes it by id word once the
  // completion fd signals (the slow path) — each yielding the same typed value the sync verb returns,
  // so a binding reuses its decode without touching the wire enums. Only the verbs the SDKs bind
  // async today (create, snapshot, status); the rest follow the same three-line shape.

  /// Begins a create, returning its request id (the async `create`'s send half).
  pub fn create_begin(&mut self, spec: &CreateSpec) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Create {
      name: spec.name.clone(),
      size: spec.size,
      names: spec.names,
      require_locked: spec.require_locked,
      base: spec.base.clone(),
    })
  }

  /// Takes a create's reply within `spin_ns` (the fast path); `None` if it has not come.
  pub fn create_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<VolumeId>, ClientError> {
    self.spin_as(id, spin_ns, extract_created)
  }

  /// Takes a create's reply by id word once the completion fd signals; `None` until it is on the ring.
  pub fn create_poll(&mut self, word: u64) -> Result<Option<VolumeId>, ClientError> {
    self.poll_as(word, extract_created)
  }

  /// Begins a snapshot, returning its request id.
  pub fn snapshot_begin(&mut self, volume: VolumeId) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Snapshot { volume })
  }

  /// Takes a snapshot's reply within `spin_ns`.
  pub fn snapshot_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<SnapshotId>, ClientError> {
    self.spin_as(id, spin_ns, extract_snapshotted)
  }

  /// Takes a snapshot's reply by id word once the completion fd signals.
  pub fn snapshot_poll(&mut self, word: u64) -> Result<Option<SnapshotId>, ClientError> {
    self.poll_as(word, extract_snapshotted)
  }

  /// Begins a status read, returning its request id.
  pub fn status_begin(&mut self, volume: VolumeId) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Status { volume })
  }

  /// Takes a status reply within `spin_ns`.
  pub fn status_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<StatusReport>, ClientError> {
    self.spin_as(id, spin_ns, extract_status)
  }

  /// Takes a status reply by id word once the completion fd signals.
  pub fn status_poll(&mut self, word: u64) -> Result<Option<StatusReport>, ClientError> {
    self.poll_as(word, extract_status)
  }

  /// Begins a list, returning its request id.
  pub fn list_begin(&mut self) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::List)
  }

  /// Takes a list reply within `spin_ns`.
  pub fn list_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<Vec<VolumeSummary>>, ClientError> {
    self.spin_as(id, spin_ns, extract_listed)
  }

  /// Takes a list reply by id word once the completion fd signals.
  pub fn list_poll(&mut self, word: u64) -> Result<Option<Vec<VolumeSummary>>, ClientError> {
    self.poll_as(word, extract_listed)
  }

  /// Begins a resize, returning its request id.
  pub fn resize_begin(
    &mut self,
    volume: VolumeId,
    size: SizeClass,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Resize { volume, size })
  }

  /// Takes a resize's reply within `spin_ns` (`true` when done).
  pub fn resize_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<bool>, ClientError> {
    self.spin_as(id, spin_ns, extract_resized)
  }

  /// Takes a resize's reply by id word once the completion fd signals.
  pub fn resize_poll(&mut self, word: u64) -> Result<Option<bool>, ClientError> {
    self.poll_as(word, extract_resized)
  }

  /// Begins a destroy, returning its request id.
  pub fn destroy_begin(&mut self, volume: VolumeId) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Destroy { volume })
  }

  /// Takes a destroy's reply within `spin_ns` (`true` when done).
  pub fn destroy_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<bool>, ClientError> {
    self.spin_as(id, spin_ns, extract_destroyed)
  }

  /// Takes a destroy's reply by id word once the completion fd signals.
  pub fn destroy_poll(&mut self, word: u64) -> Result<Option<bool>, ClientError> {
    self.poll_as(word, extract_destroyed)
  }

  /// Begins a create-green from scratch, returning its request id.
  pub fn create_green_begin(
    &mut self,
    name: &str,
    require_evidence: bool,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::CreateGreen {
      name: name.to_owned(),
      require_evidence,
      base: None,
    })
  }

  /// Begins a create-green over a complete immutable base (§4.16), returning its request id.
  pub fn create_green_over_begin(
    &mut self,
    name: &str,
    require_evidence: bool,
    base: GreenBase,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::CreateGreen {
      name: name.to_owned(),
      require_evidence,
      base: Some(base),
    })
  }

  /// Takes a create-green's reply within `spin_ns`.
  pub fn create_green_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<VolumeId>, ClientError> {
    self.spin_as(id, spin_ns, extract_green_created)
  }

  /// Takes a create-green's reply by id word once the completion fd signals.
  pub fn create_green_poll(&mut self, word: u64) -> Result<Option<VolumeId>, ClientError> {
    self.poll_as(word, extract_green_created)
  }

  /// Begins a create-work over a green, returning its request id.
  pub fn create_work_begin(
    &mut self,
    green: VolumeId,
    name: &str,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::CreateWork {
      green,
      name: name.to_owned(),
    })
  }

  /// Takes a create-work's reply (id and base version) within `spin_ns`.
  pub fn create_work_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<(VolumeId, u64)>, ClientError> {
    self.spin_as(id, spin_ns, extract_work_created)
  }

  /// Takes a create-work's reply by id word once the completion fd signals.
  pub fn create_work_poll(&mut self, word: u64) -> Result<Option<(VolumeId, u64)>, ClientError> {
    self.poll_as(word, extract_work_created)
  }

  /// Begins an edit on a work volume, returning its request id.
  pub fn edit_begin(
    &mut self,
    work: VolumeId,
    path: &str,
    at: u64,
    delete_len: u64,
    bytes: &[u8],
  ) -> Result<RequestId, ClientError> {
    let request = RequestBody::Edit {
      work,
      path: path.to_owned(),
      at,
      delete_len,
      bytes: bytes.to_vec(),
    };
    if slates_ipc::protocol::fits(self.end.region(), &request) {
      return self.begin(&request);
    }
    // Too large for one request: the async counterpart of the sync `stage` (one edit, one journal operation), run
    // as a chain the caller's polls advance; its word is the chain's first step's.
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let id = self.begin(&RequestBody::StageBegin { work, len })?;
    self.edit_chains.insert(
      id.word(),
      EditChain {
        work,
        path: path.to_owned(),
        at,
        delete_len,
        bytes: bytes.to_vec(),
        step: ChainStep::Begin,
        token: 0,
        offset: 0,
      },
    );
    Ok(id)
  }

  /// Takes an edit's reply within `spin_ns` (`true` when done).
  pub fn edit_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<bool>, ClientError> {
    if self.edit_chains.contains_key(&id.word()) {
      // A chained edit has no fast path: its polls advance it step by step.
      return Ok(None);
    }
    self.spin_as(id, spin_ns, extract_edited)
  }

  /// Takes an edit's reply by id word once the completion fd signals. A chained edit advances one step per reply
  /// and answers `None` until its last step's reply.
  pub fn edit_poll(&mut self, word: u64) -> Result<Option<bool>, ClientError> {
    if !self.edit_chains.contains_key(&word) {
      return self.poll_as(word, extract_edited);
    }
    self.take_ready()?;
    let Some(pos) = self.pending.iter().position(|(held, _)| *held == word) else {
      return Ok(None);
    };
    let (_, body) = self.pending.remove(pos);
    self.awaited.remove(&word);
    match resolved(body) {
      Ok(body) => self.advance_chain(word, body),
      Err(refused) => {
        self.edit_chains.remove(&word);
        Err(refused)
      }
    }
  }

  /// Advances the chained edit `word` by the reply `body` to its step in flight: sends the next step, or ends the
  /// chain on the final edit's reply.
  fn advance_chain(&mut self, word: u64, body: ReplyBody) -> Result<Option<bool>, ClientError> {
    let Some(chain) = self.edit_chains.get_mut(&word) else {
      return Ok(None);
    };
    match (chain.step, body) {
      (ChainStep::Begin, ReplyBody::Staged { token }) => {
        chain.token = token;
        chain.step = ChainStep::Put;
      }
      (ChainStep::Put, ReplyBody::StagePutDone { .. }) => {}
      (ChainStep::Commit, ReplyBody::Edited) => {
        self.edit_chains.remove(&word);
        return Ok(Some(true));
      }
      _ => {
        self.edit_chains.remove(&word);
        return Err(ClientError::UnexpectedReply { verb: "edit" });
      }
    }
    let next = self.next_chain_step(word)?;
    let id = match next.as_ref().map(|request| self.begin(request)) {
      Some(Ok(id)) => id,
      Some(Err(e)) => {
        self.edit_chains.remove(&word);
        return Err(e);
      }
      None => return Ok(None),
    };
    self.chain_steps.insert(id.word(), word);
    Ok(None)
  }

  /// The next request of chained edit `word`: the next page of its bytes (each what one request holds past a
  /// put's fixed framing), or, once every page is put, the edit itself.
  fn next_chain_step(&mut self, word: u64) -> Result<Option<RequestBody>, ClientError> {
    let capacity = slates_ipc::protocol::chunk_capacity(self.end.region());
    let Some(chain) = self.edit_chains.get_mut(&word) else {
      return Ok(None);
    };
    if chain.offset >= chain.bytes.len() {
      chain.step = ChainStep::Commit;
      return Ok(Some(RequestBody::EditStaged {
        work: chain.work,
        path: chain.path.clone(),
        at: chain.at,
        delete_len: chain.delete_len,
        token: chain.token,
      }));
    }
    let empty = RequestBody::StagePut {
      work: chain.work,
      token: chain.token,
      offset: u64::MAX,
      bytes: Vec::new(),
    };
    let page = capacity
      .saturating_sub(slates_ipc::protocol::framed_len(&empty))
      .max(1);
    let end = chain.offset.saturating_add(page).min(chain.bytes.len());
    let request = RequestBody::StagePut {
      work: chain.work,
      token: chain.token,
      offset: u64::try_from(chain.offset).unwrap_or(u64::MAX),
      bytes: chain
        .bytes
        .get(chain.offset..end)
        .unwrap_or_default()
        .to_vec(),
    };
    chain.offset = end;
    Ok(Some(request))
  }

  /// Begins a submit of a work volume with no evidence, returning its request id.
  pub fn submit_begin(&mut self, work: VolumeId) -> Result<RequestId, ClientError> {
    self.submit_with_evidence_begin(work, &[])
  }

  /// Begins a submit of a work volume carrying `evidence` references (§4.16), returning its request id.
  pub fn submit_with_evidence_begin(
    &mut self,
    work: VolumeId,
    evidence: &[[u8; 32]],
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Submit {
      work,
      evidence: evidence.to_vec(),
    })
  }

  /// Begins an advance of a green attachment (§4.16), returning its request id.
  pub fn advance_begin(
    &mut self,
    attachment: u64,
    version: Option<u64>,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Advance {
      attachment,
      version,
    })
  }

  /// Takes an advance's outcome within `spin_ns`.
  pub fn advance_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<Advanced>, ClientError> {
    self.spin_as(id, spin_ns, extract_advanced)
  }

  /// Takes an advance's outcome by id word once the completion fd signals.
  pub fn advance_poll(&mut self, word: u64) -> Result<Option<Advanced>, ClientError> {
    self.poll_as(word, extract_advanced)
  }

  /// Begins a read of a file at a view (§4.12 `read`), returning its request id.
  pub fn read_begin(
    &mut self,
    volume: VolumeId,
    path: &str,
    at: ReadAt,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Read {
      volume,
      path: path.to_owned(),
      at,
    })
  }

  /// Takes a read's bytes within `spin_ns`.
  pub fn read_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<Vec<u8>>, ClientError> {
    self.spin_as(id, spin_ns, extract_read)
  }

  /// Takes a read's bytes by id word once the completion fd signals.
  pub fn read_poll(&mut self, word: u64) -> Result<Option<Vec<u8>>, ClientError> {
    self.poll_as(word, extract_read)
  }

  /// Takes a submit's outcome within `spin_ns`.
  pub fn submit_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<Submitted>, ClientError> {
    self.spin_as(id, spin_ns, extract_submitted)
  }

  /// Takes a submit's outcome by id word once the completion fd signals.
  pub fn submit_poll(&mut self, word: u64) -> Result<Option<Submitted>, ClientError> {
    self.poll_as(word, extract_submitted)
  }

  /// Begins a versions query on a green, returning its request id.
  pub fn versions_begin(&mut self, green: VolumeId) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Versions { green })
  }

  /// Takes a versions reply within `spin_ns`.
  pub fn versions_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<u64>, ClientError> {
    self.spin_as(id, spin_ns, extract_versions)
  }

  /// Takes a versions reply by id word once the completion fd signals.
  pub fn versions_poll(&mut self, word: u64) -> Result<Option<u64>, ClientError> {
    self.poll_as(word, extract_versions)
  }

  /// Begins a changed-since query on a green, returning its request id.
  pub fn changed_since_begin(
    &mut self,
    green: VolumeId,
    version: u64,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::ChangedSince { green, version })
  }

  /// Takes a changed-since reply within `spin_ns`.
  pub fn changed_since_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<Vec<String>>, ClientError> {
    self.spin_as(id, spin_ns, extract_changed)
  }

  /// Takes a changed-since reply by id word once the completion fd signals.
  pub fn changed_since_poll(&mut self, word: u64) -> Result<Option<Vec<String>>, ClientError> {
    self.poll_as(word, extract_changed)
  }

  /// Begins a rebase of a work volume, returning its request id.
  pub fn rebase_begin(&mut self, work: VolumeId) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Rebase { work })
  }

  /// Takes a rebase's outcome within `spin_ns`.
  pub fn rebase_spin(
    &mut self,
    id: RequestId,
    spin_ns: u64,
  ) -> Result<Option<Rebased>, ClientError> {
    self.spin_as(id, spin_ns, extract_rebased)
  }

  /// Takes a rebase's outcome by id word once the completion fd signals.
  pub fn rebase_poll(&mut self, word: u64) -> Result<Option<Rebased>, ClientError> {
    self.poll_as(word, extract_rebased)
  }

  /// Begins a namespace declaration on a work volume, returning its request id. The `WorkOp` is the
  /// same one the sync [`Self::declare`] takes; a binding builds it and never crosses it over FFI.
  pub fn declare_begin(&mut self, work: VolumeId, op: WorkOp) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Declare { work, op })
  }

  /// Takes a declaration's reply within `spin_ns` (`true` when done).
  pub fn declare_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<bool>, ClientError> {
    self.spin_as(id, spin_ns, extract_declared)
  }

  /// Takes a declaration's reply by id word once the completion fd signals.
  pub fn declare_poll(&mut self, word: u64) -> Result<Option<bool>, ClientError> {
    self.poll_as(word, extract_declared)
  }

  /// Begins a landing (§4.15), returning its request id. The SDK creates no grant itself (R10): a
  /// landing without a satisfying `grant` comes back `GrantRequired` for a human to authorize.
  pub fn land_begin(
    &mut self,
    volume: VolumeId,
    snapshot: Option<SnapshotId>,
    target: &str,
    filter: Filter,
    grant: Option<u64>,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Land {
      volume,
      snapshot,
      target: target.to_owned(),
      filter,
      grant,
    })
  }

  /// Takes a landing's outcome within `spin_ns`.
  pub fn land_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<Landing>, ClientError> {
    self.spin_as(id, spin_ns, extract_landed)
  }

  /// Takes a landing's outcome by id word once the completion fd signals.
  pub fn land_poll(&mut self, word: u64) -> Result<Option<Landing>, ClientError> {
    self.poll_as(word, extract_landed)
  }

  /// Takes `id`'s reply within `spin_ns` and extracts its typed value (the fast path over a typed verb).
  fn spin_as<T>(
    &mut self,
    id: RequestId,
    spin_ns: u64,
    extract: fn(ReplyBody) -> Result<T, ClientError>,
  ) -> Result<Option<T>, ClientError> {
    match self.spin_reply(id, spin_ns)? {
      Some(body) => extract(body).map(Some),
      None => Ok(None),
    }
  }

  /// Takes the reply for `word` if present and extracts its typed value (the slow path over a typed verb).
  fn poll_as<T>(
    &mut self,
    word: u64,
    extract: fn(ReplyBody) -> Result<T, ClientError>,
  ) -> Result<Option<T>, ClientError> {
    match self.poll_reply_word(word)? {
      Some(body) => extract(body).map(Some),
      None => Ok(None),
    }
  }

  /// Writes the request into the command ring, waiting on credit (the daemon drains the ring
  /// in microseconds; a ring that stays full past the reply deadline is a stalled or gone
  /// daemon, handled as a stalled reply is). `true` once written; `false` when the daemon was
  /// found gone instead and the client reconnected (the caller binds and sends again).
  /// One attempt to put `body` under `id` on the command ring: `Ok(false)` when it has no slot.
  fn try_send(&mut self, id: RequestId, body: &RequestBody) -> Result<bool, ClientError> {
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      Direction::Request,
      index,
      id.word(),
      body,
    )?;
    match self.end.send(&slot) {
      Ok(()) => Ok(true),
      Err(IpcError::RingFull) => Ok(false),
      Err(e) => Err(ClientError::Ipc(e)),
    }
  }

  fn send(&mut self, id: RequestId, body: &RequestBody) -> Result<bool, ClientError> {
    let started = Instant::now();
    loop {
      let index = self.end.next_request_index();
      let slot = pack(
        self.end.region_mut(),
        Direction::Request,
        index,
        id.word(),
        body,
      )?;
      match self.end.send(&slot) {
        Ok(()) => return Ok(true),
        Err(IpcError::RingFull) => {
          if elapsed_ns(started) < self.deadlines.reply_ns {
            std::hint::spin_loop();
            continue;
          }
          if self.end.daemon_gone() {
            self.reconnect()?;
            return Ok(false);
          }
          return Err(ClientError::Stalled {
            after_ns: self.deadlines.reply_ns,
          });
        }
        Err(e) => return Err(ClientError::Ipc(e)),
      }
    }
  }

  /// Reconnects under the client's id inside the reconnect budget; refused when the budget
  /// passes or a live client holds the id. The fresh channel is the account's until the client
  /// binds it again (`rebind_if_needed`), which every send path does before sending.
  fn reconnect(&mut self) -> Result<(), ClientError> {
    let started = Instant::now();
    let budget = self.deadlines.reconnect_ns;
    let mut pause_ns = self.end.wake_estimate_ns().max(1);
    loop {
      match connect_as(&self.instance, self.client_id, self.deadlines.reconnect_ns) {
        Ok(connected) => {
          let assigned = connected.region.client_id();
          if assigned != self.client_id {
            return Err(ClientError::SessionTaken { assigned });
          }
          self.end = ClientEnd::connected(connected);
          self.reconnects = self.reconnects.saturating_add(1);
          self.bound = false;
          self.attest_in_flight = None;
          return Ok(());
        }
        Err(IpcError::DaemonUnavailable { .. } | IpcError::RingFull) => {
          if elapsed_ns(started) >= budget {
            return Err(ClientError::DaemonGone { after_ns: budget });
          }
          std::thread::park_timeout(std::time::Duration::from_nanos(pause_ns));
          pause_ns = next_pause_ns(pause_ns, budget);
        }
        Err(e) => return Err(ClientError::Ipc(e)),
      }
    }
  }

  /// Creates a volume; the id.
  pub fn create(&mut self, spec: &CreateSpec) -> Result<VolumeId, ClientError> {
    match self.call(&RequestBody::Create {
      name: spec.name.clone(),
      size: spec.size,
      names: spec.names,
      require_locked: spec.require_locked,
      base: spec.base.clone(),
    })? {
      ReplyBody::Created { id } => Ok(id),
      _ => Err(ClientError::UnexpectedReply { verb: "create" }),
    }
  }

  /// Takes a snapshot; its id.
  pub fn snapshot(&mut self, volume: VolumeId) -> Result<SnapshotId, ClientError> {
    self.snapshot_with_coverage(volume).map(|(id, _)| id)
  }

  /// Takes a snapshot and returns what it covers (§4.6 "Writeback and snapshot barrier"): the boundary
  /// the barrier could claim and the attachments whose generation it closed.
  pub fn snapshot_with_coverage(
    &mut self,
    volume: VolumeId,
  ) -> Result<(SnapshotId, SnapshotCoverage), ClientError> {
    match self.call(&RequestBody::Snapshot { volume })? {
      ReplyBody::Snapshotted { id, coverage } => Ok((id, coverage)),
      _ => Err(ClientError::UnexpectedReply { verb: "snapshot" }),
    }
  }

  /// Destroys a snapshot, returning its retained versions to the shard; refused while a clone pins it.
  pub fn destroy_snapshot(
    &mut self,
    volume: VolumeId,
    snapshot: SnapshotId,
  ) -> Result<(), ClientError> {
    match self.call(&RequestBody::DestroySnapshot { volume, snapshot })? {
      ReplyBody::SnapshotDestroyed => Ok(()),
      _ => Err(ClientError::UnexpectedReply {
        verb: "destroy_snapshot",
      }),
    }
  }

  /// Promotes a lost region's declared mirror on the root group (§4.8, D-14 — operator-initiated region-loss
  /// promotion). `Ok` once the promotion is accepted: it commits on the root group and every node re-homes the
  /// lost region's volumes to the mirror. Refused `NotRootLeader` when this daemon's root group is not the
  /// leader — the operator re-issues it on the leader (`status` names it) — or `Unsupported` when the region
  /// has no declared mirror. The operator issues this only after judging the region truly lost; a merely
  /// partitioned region is never failed over automatically, so its mirror is not promoted into a second owner.
  pub fn promote_region(&mut self, region: u64) -> Result<(), ClientError> {
    match self.call(&RequestBody::PromoteRegion { region })? {
      ReplyBody::Acknowledged => Ok(()),
      _ => Err(ClientError::UnexpectedReply {
        verb: "promote_region",
      }),
    }
  }

  /// Creates a green volume from scratch — a shared merge target (§4.16); its id.
  pub fn create_green(
    &mut self,
    name: &str,
    require_evidence: bool,
  ) -> Result<VolumeId, ClientError> {
    self.create_green_with(name, require_evidence, None)
  }

  /// Creates a green volume whose version 0 is a complete immutable base — a snapshot of a volume
  /// (§4.16; refused `ConsistentBaseUnavailable` for a snapshot still served live from a host
  /// directory); its id.
  pub fn create_green_over(
    &mut self,
    name: &str,
    require_evidence: bool,
    base: GreenBase,
  ) -> Result<VolumeId, ClientError> {
    self.create_green_with(name, require_evidence, Some(base))
  }

  fn create_green_with(
    &mut self,
    name: &str,
    require_evidence: bool,
    base: Option<GreenBase>,
  ) -> Result<VolumeId, ClientError> {
    match self.call(&RequestBody::CreateGreen {
      name: name.to_owned(),
      require_evidence,
      base,
    })? {
      ReplyBody::GreenCreated { id } => Ok(id),
      _ => Err(ClientError::UnexpectedReply {
        verb: "create_green",
      }),
    }
  }

  /// Re-pins a green attachment to `version`, or to the head (§4.16 `advance`): the version now
  /// pinned and the paths the move invalidated.
  pub fn advance(
    &mut self,
    attachment: u64,
    version: Option<u64>,
  ) -> Result<Advanced, ClientError> {
    let reply = self.call(&RequestBody::Advance {
      attachment,
      version,
    })?;
    extract_advanced(reply)
  }

  /// Reads a file's bytes at a view (§4.12 `read`): a green's head or named version, the version
  /// an attachment pins, or a work's or plain volume's live tree.
  pub fn read(&mut self, volume: VolumeId, path: &str, at: ReadAt) -> Result<Vec<u8>, ClientError> {
    // A reply rides one bulk chunk, so the file comes in pages (`ReadRange`), each as large as the daemon's reply
    // chunk allows; every page must carry the first one's stamp, or the file changed between them and the bytes
    // would mix two states of it.
    let mut bytes = Vec::new();
    let mut first_stamp = None;
    loop {
      let offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
      let reply = self.call(&RequestBody::ReadRange {
        volume,
        path: path.to_owned(),
        at,
        offset,
        max: u64::MAX,
      })?;
      let ReplyBody::ReadPage {
        bytes: page,
        total,
        stamp,
      } = resolved(reply)?
      else {
        return Err(ClientError::UnexpectedReply { verb: "read" });
      };
      if *first_stamp.get_or_insert(stamp) != stamp {
        return Err(ClientError::ChangedWhileRead {
          path: path.to_owned(),
        });
      }
      let done = offset.saturating_add(u64::try_from(page.len()).unwrap_or(u64::MAX)) >= total;
      if page.is_empty() && !done {
        // A page that makes no progress short of the end would loop forever.
        return Err(ClientError::UnexpectedReply { verb: "read" });
      }
      bytes.extend_from_slice(&page);
      if done {
        return Ok(bytes);
      }
    }
  }

  /// A directory's direct entries at a view (§4.12 `slates.fs.list`), every page of them (`ReadDir`): a green's
  /// head or named version, the version an attachment pins, or a work's or plain volume's live tree.
  pub fn list_dir(
    &mut self,
    volume: VolumeId,
    path: &str,
    at: ReadAt,
  ) -> Result<Vec<slates_ipc::protocol::DirEntry>, ClientError> {
    let mut entries = Vec::new();
    let mut cursor = 0u64;
    loop {
      let reply = self.call(&RequestBody::ReadDir {
        volume,
        path: path.to_owned(),
        at,
        cursor,
      })?;
      let ReplyBody::DirPage {
        entries: page,
        next,
      } = resolved(reply)?
      else {
        return Err(ClientError::UnexpectedReply { verb: "list" });
      };
      let progressed = !page.is_empty();
      entries.extend(page);
      match next {
        None => return Ok(entries),
        // A page that names its own cursor and holds nothing would loop forever.
        Some(next) if next == cursor && !progressed => {
          return Err(ClientError::UnexpectedReply { verb: "list" });
        }
        Some(next) => cursor = next,
      }
    }
  }

  /// A green's head version (§4.16 merge chain).
  pub fn versions(&mut self, green: VolumeId) -> Result<u64, ClientError> {
    match self.call(&RequestBody::Versions { green })? {
      ReplyBody::Versions { head } => Ok(head),
      _ => Err(ClientError::UnexpectedReply { verb: "versions" }),
    }
  }

  /// The files a green changed strictly after `version` (§4.16).
  pub fn changed_since(
    &mut self,
    green: VolumeId,
    version: u64,
  ) -> Result<Vec<String>, ClientError> {
    match self.call(&RequestBody::ChangedSince { green, version })? {
      ReplyBody::ChangedSince { paths } => Ok(paths),
      _ => Err(ClientError::UnexpectedReply {
        verb: "changed_since",
      }),
    }
  }

  /// Creates a work volume over a green (§4.16); its id and the green version it is based on.
  pub fn create_work(
    &mut self,
    green: VolumeId,
    name: &str,
  ) -> Result<(VolumeId, u64), ClientError> {
    match self.call(&RequestBody::CreateWork {
      green,
      name: name.to_owned(),
    })? {
      ReplyBody::WorkCreated { id, base } => Ok((id, base)),
      _ => Err(ClientError::UnexpectedReply {
        verb: "create_work",
      }),
    }
  }

  /// Stages `bytes` for an edit of `work` (`StageBegin`, then `StagePut` in pages each one request holds): the
  /// staging token.
  fn stage(&mut self, work: VolumeId, bytes: &[u8]) -> Result<u64, ClientError> {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let token = match resolved(self.call(&RequestBody::StageBegin { work, len })?)? {
      ReplyBody::Staged { token } => token,
      _ => return Err(ClientError::UnexpectedReply { verb: "stage" }),
    };
    // Each put's page is what a request holds past the put's own framing at its largest offset and length.
    let empty = RequestBody::StagePut {
      work,
      token,
      offset: u64::MAX,
      bytes: Vec::new(),
    };
    // Every field of a put is fixed width but its bytes (and their length prefix is a fixed four bytes), so the
    // empty put's framed length is exactly the framing of every put.
    let framing = slates_ipc::protocol::framed_len(&empty);
    let page = slates_ipc::protocol::chunk_capacity(self.end.region())
      .saturating_sub(framing)
      .max(1);
    let mut offset = 0usize;
    for piece in bytes.chunks(page) {
      let at = u64::try_from(offset).unwrap_or(u64::MAX);
      match resolved(self.call(&RequestBody::StagePut {
        work,
        token,
        offset: at,
        bytes: piece.to_vec(),
      })?)? {
        ReplyBody::StagePutDone { .. } => {}
        _ => return Err(ClientError::UnexpectedReply { verb: "stage" }),
      }
      offset = offset.saturating_add(piece.len());
    }
    Ok(token)
  }

  /// Declares an edit on a work volume (§4.16): a splice at `path` — remove `delete_len` bytes at
  /// `at`, insert `bytes`. An edit larger than one request is staged in pages and applied as one
  /// edit.
  pub fn edit(
    &mut self,
    work: VolumeId,
    path: &str,
    at: u64,
    delete_len: u64,
    bytes: &[u8],
  ) -> Result<(), ClientError> {
    let request = RequestBody::Edit {
      work,
      path: path.to_owned(),
      at,
      delete_len,
      bytes: bytes.to_vec(),
    };
    let reply = if slates_ipc::protocol::fits(self.end.region(), &request) {
      self.call(&request)?
    } else {
      // Too large for one request: staged on the work's owner in pages, then applied as one edit (§4.16: one
      // edit stays one journal operation).
      let token = self.stage(work, bytes)?;
      self.call(&RequestBody::EditStaged {
        work,
        path: path.to_owned(),
        at,
        delete_len,
        token,
      })?
    };
    match reply {
      ReplyBody::Edited => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "edit" }),
    }
  }

  /// Replaces a plain volume's file at `path` with `bytes` under the caller's write `attachment` (§4.12 `fs.write`):
  /// created with `mode` when absent, else truncated and rewritten. Content too large for one request is staged on the
  /// volume's owner first and written as one verb, so the file never holds part of it. The file's new size.
  pub fn fs_write(
    &mut self,
    (volume, attachment): (VolumeId, u64),
    path: &str,
    bytes: &[u8],
    mode: u32,
  ) -> Result<u64, ClientError> {
    let request = RequestBody::FsWrite {
      volume,
      attachment,
      path: path.to_owned(),
      bytes: bytes.to_vec(),
      mode,
    };
    let reply = if slates_ipc::protocol::fits(self.end.region(), &request) {
      self.call(&request)?
    } else {
      let token = self.stage(volume, bytes)?;
      self.call(&RequestBody::FsWriteStaged {
        volume,
        attachment,
        path: path.to_owned(),
        token,
        mode,
      })?
    };
    fs_done(reply, "fs write")
  }

  /// Removes the file, symbolic link or empty directory at `path` (§4.12 `fs.remove`).
  pub fn fs_remove(
    &mut self,
    (volume, attachment): (VolumeId, u64),
    path: &str,
  ) -> Result<(), ClientError> {
    let reply = self.call(&RequestBody::FsRemove {
      volume,
      attachment,
      path: path.to_owned(),
    })?;
    fs_done(reply, "fs remove").map(|_| ())
  }

  /// Renames `from` to `to`, replacing what `to` names (§4.12 `fs.move`). The moved object's size.
  pub fn fs_rename(
    &mut self,
    (volume, attachment): (VolumeId, u64),
    from: &str,
    to: &str,
  ) -> Result<u64, ClientError> {
    let reply = self.call(&RequestBody::FsRename {
      volume,
      attachment,
      from: from.to_owned(),
      to: to.to_owned(),
    })?;
    fs_done(reply, "fs rename")
  }

  /// Makes the directory `path` with `mode` (§4.12 `fs.mkdir`).
  pub fn fs_mkdir(
    &mut self,
    (volume, attachment): (VolumeId, u64),
    path: &str,
    mode: u32,
  ) -> Result<(), ClientError> {
    let reply = self.call(&RequestBody::FsMkdir {
      volume,
      attachment,
      path: path.to_owned(),
      mode,
    })?;
    fs_done(reply, "fs mkdir").map(|_| ())
  }

  /// Declares a namespace or metadata operation on a work volume (§4.16): the counterpart to `edit`'s
  /// content splice — an unlink, rename, directory, mode, symlink, hard link or extended attribute.
  pub fn declare(&mut self, work: VolumeId, op: WorkOp) -> Result<(), ClientError> {
    match self.call(&RequestBody::Declare { work, op })? {
      ReplyBody::Declared => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "declare" }),
    }
  }

  /// Submits a work volume's declared operations to its green (§4.16) with no evidence: accepted at
  /// a new version, or a conflict with windows to rebase.
  pub fn submit(&mut self, work: VolumeId) -> Result<Submitted, ClientError> {
    self.submit_with_evidence(work, &[])
  }

  /// Submits a work volume's declared operations carrying `evidence` references (§4.16: opaque to
  /// slates; a green that requires evidence refuses `EvidenceRequired` without any).
  pub fn submit_with_evidence(
    &mut self,
    work: VolumeId,
    evidence: &[[u8; 32]],
  ) -> Result<Submitted, ClientError> {
    match self.call(&RequestBody::Submit {
      work,
      evidence: evidence.to_vec(),
    })? {
      ReplyBody::Submitted {
        version: Some(v), ..
      } => Ok(Submitted::Accepted(v)),
      ReplyBody::Submitted {
        version: None,
        conflicts,
      } => Ok(Submitted::Conflict(conflicts)),
      _ => Err(ClientError::UnexpectedReply { verb: "submit" }),
    }
  }

  /// Rebases a work volume onto its green's head (§4.16): its pending operations are mapped forward,
  /// moving the work's base without committing to the green — the head version it now sits on, or the
  /// conflict windows to resolve first.
  pub fn rebase(&mut self, work: VolumeId) -> Result<Rebased, ClientError> {
    match self.call(&RequestBody::Rebase { work })? {
      ReplyBody::Rebased {
        version: Some(v), ..
      } => Ok(Rebased::Rebased(v)),
      ReplyBody::Rebased {
        version: None,
        conflicts,
      } => Ok(Rebased::Conflict(conflicts)),
      _ => Err(ClientError::UnexpectedReply { verb: "rebase" }),
    }
  }

  /// Clones a snapshot into a new volume; the clone's id.
  pub fn clone_snapshot(
    &mut self,
    volume: VolumeId,
    snapshot: SnapshotId,
    name: &str,
  ) -> Result<VolumeId, ClientError> {
    match self.call(&RequestBody::Clone {
      volume,
      snapshot,
      name: name.to_owned(),
    })? {
      ReplyBody::Cloned { id } => Ok(id),
      _ => Err(ClientError::UnexpectedReply { verb: "clone" }),
    }
  }

  /// Attaches in the record form under the root mount (a write intent takes the lease).
  pub fn attach(
    &mut self,
    volume: VolumeId,
    snapshot: Option<SnapshotId>,
    intent: Intent,
  ) -> Result<Attachment, ClientError> {
    self.attach_with(volume, snapshot, intent, AttachRequest::Root)
  }

  /// Attaches for a host kernel mount of `volume` (§4.6, §4.13; the `slates mount` flow): the attachment
  /// is the **mount's** — it outlives this client and the daemon, ending with the kernel's `UMNT` of the
  /// mount, a `detach`, or the volume's destroy — and the reply's `token` is the capability the mount
  /// presents (`/<name>@<attachment_hex>.<token_hex>`). A write intent takes the volume's write lease as
  /// any write attachment does (D-16; refused `LeaseHeld` while another principal holds it unexpired);
  /// a read intent records a read-only attachment, whose capability presents a read-only view.
  pub fn attach_mount(
    &mut self,
    volume: VolumeId,
    intent: Intent,
  ) -> Result<Attachment, ClientError> {
    self.attach_with(volume, None, intent, AttachRequest::HostMount)
  }

  /// Attaches for a host kernel mount presenting only directory `subtree` of `volume` (§4.6 scoped exports;
  /// AUD-29-76): the attachment is the mount's, as [`Client::attach_mount`]'s, and its capability reaches
  /// nothing outside that directory — the export's root is it and `..` there is itself, on every request.
  pub fn attach_scoped_mount(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    subtree: &str,
  ) -> Result<Attachment, ClientError> {
    self.attach_with(
      volume,
      None,
      intent,
      AttachRequest::ScopedHostMount {
        subtree: subtree.to_owned(),
      },
    )
  }

  /// Attaches a Linux FUSE mount of `volume` at `mount_point` (§4.6 "Linux"; AUD-29-64): the daemon mounts
  /// through the OS's `fusermount3` and serves the mount on the volume's owner shard; the reply comes once
  /// the mount is established (waited for while the daemon lives, as a granted landing's is), with the
  /// mount point as its `path`. The attachment is the mount's: it ends with the kernel's unmount, a
  /// `detach` (which unmounts), or the volume's destroy. Refused typed where FUSE is not offered.
  pub fn attach_fuse(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    mount_point: &str,
  ) -> Result<Attachment, ClientError> {
    self.attach_with(
      volume,
      None,
      intent,
      AttachRequest::FuseMount {
        mount_point: mount_point.to_owned(),
      },
    )
  }

  /// Attaches a Linux FUSE mount at `mount_point` presenting only the directory `subtree` of `volume` (§4.6
  /// scoped exports; AUD-29-76): an `attach_fuse` whose root is that directory and which reaches nothing
  /// outside it. Refused typed when `subtree` names nothing or no directory, or where FUSE is not offered.
  pub fn attach_scoped_fuse(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    mount_point: &str,
    subtree: &str,
  ) -> Result<Attachment, ClientError> {
    self.attach_with(
      volume,
      None,
      intent,
      AttachRequest::ScopedFuseMount {
        mount_point: mount_point.to_owned(),
        subtree: subtree.to_owned(),
      },
    )
  }

  /// Attaches a Linux FUSE mount at `mount_point` that other local users' processes may reach (`allow_other`):
  /// what a container runtime binds into a container (§4.6 A-9). With `subtree`, only that directory. Refused
  /// `AttachmentUnsupported{Fuse, AllowOtherNotGranted}` where the host's operator has not granted
  /// `user_allow_other`.
  pub fn attach_shared_fuse(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    mount_point: &str,
    subtree: Option<&str>,
  ) -> Result<Attachment, ClientError> {
    self.attach_with(
      volume,
      None,
      intent,
      AttachRequest::SharedFuseMount {
        mount_point: mount_point.to_owned(),
        subtree: subtree.map(str::to_owned),
      },
    )
  }

  /// Attaches in `form` (§4.4 `attach(volume|snapshot, consumer, transport, chosen_path?)`; §4.6
  /// A-9): the daemon establishes the form and reports it with the transport's capability, or refuses
  /// the form typed (`Refusal::AttachmentUnsupported{transport, reason}`) before any effect.
  pub fn attach_with(
    &mut self,
    volume: VolumeId,
    snapshot: Option<SnapshotId>,
    intent: Intent,
    form: AttachRequest,
  ) -> Result<Attachment, ClientError> {
    match self.call(&RequestBody::Attach {
      volume,
      snapshot,
      intent,
      form,
    })? {
      ReplyBody::Attached {
        attachment,
        lease_epoch,
        path,
        version,
        established,
        capability,
        token,
      } => Ok(Attachment {
        attachment,
        lease_epoch,
        path,
        version,
        established,
        capability,
        token,
      }),
      _ => Err(ClientError::UnexpectedReply { verb: "attach" }),
    }
  }

  /// Detaches.
  pub fn detach(&mut self, attachment: u64) -> Result<(), ClientError> {
    match self.call(&RequestBody::Detach { attachment })? {
      ReplyBody::Detached => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "detach" }),
    }
  }

  /// Binds a host mount's attachment to the path the mount was established at (§4.4 `Binding →
  /// Bound`; GAP-A9-4): the record's form becomes the chosen path, which `status` then reports.
  /// Refused for an attachment that is not a host mount's, or another principal's.
  pub fn bind_mount(&mut self, attachment: u64, path: &str) -> Result<(), ClientError> {
    match self.call(&RequestBody::BindMount {
      attachment,
      path: path.to_owned(),
    })? {
      ReplyBody::MountBound => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "bind_mount" }),
    }
  }

  /// Resizes.
  pub fn resize(&mut self, volume: VolumeId, size: SizeClass) -> Result<(), ClientError> {
    match self.call(&RequestBody::Resize { volume, size })? {
      ReplyBody::Resized => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "resize" }),
    }
  }

  /// Destroys (the daemon tears the volume down in cooperative slices after replying).
  pub fn destroy(&mut self, volume: VolumeId) -> Result<(), ClientError> {
    match self.call(&RequestBody::Destroy { volume })? {
      ReplyBody::Destroyed => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "destroy" }),
    }
  }

  /// The status report.
  pub fn status(&mut self, volume: VolumeId) -> Result<StatusReport, ClientError> {
    match self.call(&RequestBody::Status { volume })? {
      ReplyBody::Status { report } => Ok(*report),
      _ => Err(ClientError::UnexpectedReply { verb: "status" }),
    }
  }

  /// The daemon's status (§4.14 `slates.status`).
  pub fn daemon_status(&mut self) -> Result<DaemonReport, ClientError> {
    match self.call(&RequestBody::DaemonStatus)? {
      ReplyBody::DaemonStatus { report } => Ok(*report),
      _ => Err(ClientError::UnexpectedReply {
        verb: "daemon_status",
      }),
    }
  }

  /// One shard's telemetry drain (§4.14): the chokepoint spans its bounded ring holds, up to one
  /// reply's quota, with the loss marker for the batch, the spans left for the next drain, and every
  /// chokepoint's freshness. A read that consumes; a retry returns the same batch (its completion is
  /// recorded like any verb).
  pub fn telemetry(&mut self, partition: u16) -> Result<TelemetryReport, ClientError> {
    match self.call(&RequestBody::Telemetry { partition })? {
      ReplyBody::Telemetry { report } => Ok(report),
      _ => Err(ClientError::UnexpectedReply { verb: "telemetry" }),
    }
  }

  /// Awaits a durability scope for a volume's head, or a snapshot (§4.8 D-18): the region
  /// commit returns whether the head is placed (true at `f = 0`, the local append); the
  /// mirror is refused `Unsupported` where none exists. Returns `(placed, mirror_age_ns)`.
  pub fn await_placed(
    &mut self,
    volume: VolumeId,
    snapshot: Option<SnapshotId>,
    scope: Scope,
  ) -> Result<(bool, Option<u64>), ClientError> {
    match self.call(&RequestBody::AwaitPlaced {
      volume,
      snapshot,
      scope,
    })? {
      ReplyBody::Placed {
        placed,
        mirror_age_ns,
      } => Ok((placed, mirror_age_ns)),
      _ => Err(ClientError::UnexpectedReply {
        verb: "await_placed",
      }),
    }
  }

  /// The caller's volumes.
  pub fn list(&mut self) -> Result<Vec<VolumeSummary>, ClientError> {
    match self.call(&RequestBody::List)? {
      ReplyBody::Listed { volumes } => Ok(volumes),
      _ => Err(ClientError::UnexpectedReply { verb: "list" }),
    }
  }

  /// Lands a snapshot's diverged entries onto a host directory (§4.15). Without a grant the
  /// result is `Landing::GrantRequired`; with one, `Landing::Landed`.
  pub fn land(
    &mut self,
    volume: VolumeId,
    snapshot: Option<SnapshotId>,
    target: &str,
    filter: Filter,
    grant: Option<u64>,
  ) -> Result<Landing, ClientError> {
    match self.call(&RequestBody::Land {
      volume,
      snapshot,
      target: target.to_owned(),
      filter,
      grant,
    })? {
      ReplyBody::Landed { outcome } => Ok(Landing::Landed(outcome)),
      ReplyBody::GrantRequired {
        landing,
        manifest,
        summary,
        conflicts,
      } => Ok(Landing::GrantRequired {
        landing,
        manifest,
        summary,
        conflicts,
      }),
      _ => Err(ClientError::UnexpectedReply { verb: "land" }),
    }
  }

  /// Issues the grant a presented landing needs, carrying the human surface's proof of issuer authority
  /// (§4.13 "Grants"; the proof is `slates_server::landing::grant_proof` under the anchor's issuer
  /// secret — only a caller that maps the anchor segment can make one). Returns the grant id `land`
  /// then consumes. Refused `GrantIssuerUnverified` when the proof does not verify, `GrantMismatch` when
  /// the approved manifest is not the presented one, `NotFound` when nothing awaits under `landing`.
  pub fn grant(
    &mut self,
    landing: u64,
    manifest: [u8; 32],
    scope: GrantScope,
    term_ns: u64,
    proof: [u8; 32],
  ) -> Result<u64, ClientError> {
    match self.call(&RequestBody::Grant {
      landing,
      manifest,
      scope,
      term_ns,
      proof,
    })? {
      ReplyBody::Granted { grant } => Ok(grant),
      _ => Err(ClientError::UnexpectedReply { verb: "grant" }),
    }
  }

  /// Enrolls a consumer under `account` (§4.13 "Principals"): the human surface's verb, carrying its
  /// proof of issuer authority (`slates_server::landing::enroll_proof` under the anchor's issuer
  /// secret). Returns the consumer id and the capability, shown once — the caller delivers it to the
  /// workload through `slates_ipc::delivery`, never through a channel other agents share. Refused
  /// `GrantIssuerUnverified` when the proof does not verify (an agent cannot enroll itself).
  pub fn enroll(
    &mut self,
    account: u32,
    proof: [u8; 32],
  ) -> Result<(u64, Capability), ClientError> {
    match self.call(&RequestBody::Enroll { account, proof })? {
      ReplyBody::Enrolled { consumer, secret } => Ok((consumer, secret)),
      _ => Err(ClientError::UnexpectedReply { verb: "enroll" }),
    }
  }

  /// Revokes a consumer's enrollment (§4.13): the human surface's verb with its proof of issuer
  /// authority (`revoke_proof`); acknowledged only once every shard has marked the consumer's bound
  /// channels, so every later verb from them refuses `ConsumerRevoked`.
  pub fn revoke(&mut self, consumer: u64, proof: [u8; 32]) -> Result<(), ClientError> {
    match self.call(&RequestBody::Revoke { consumer, proof })? {
      ReplyBody::Revoked => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "revoke" }),
    }
  }

  /// Sets a principal's rights on a volume (§4.13 "Access lists"; `admin` on the volume is required;
  /// rights all false remove the entry; the owner's rights are not an entry).
  pub fn share(
    &mut self,
    volume: VolumeId,
    principal: Principal,
    rights: Rights,
  ) -> Result<(), ClientError> {
    match self.call(&RequestBody::Share {
      volume,
      principal,
      rights,
    })? {
      ReplyBody::Shared => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "share" }),
    }
  }

  /// The caller's grants.
  pub fn grants(&mut self) -> Result<Vec<GrantSummary>, ClientError> {
    match self.call(&RequestBody::Grants)? {
      ReplyBody::Grants { grants } => Ok(grants),
      _ => Err(ClientError::UnexpectedReply { verb: "grants" }),
    }
  }

  /// The audit log from `since`.
  pub fn audit(&mut self, since: u64) -> Result<Vec<AuditEntry>, ClientError> {
    match self.call(&RequestBody::Audit { since })? {
      ReplyBody::Audit { records } => Ok(records),
      _ => Err(ClientError::UnexpectedReply { verb: "audit" }),
    }
  }

  /// Acknowledges every completion up to `up_to` (releases their records) — held below an
  /// unpublished id awaiting its retry, whatever `up_to` asks (AC-2.3; AUD-06).
  pub fn acknowledge(&mut self, up_to: u32) -> Result<(), ClientError> {
    let up_to = up_to.min(self.ack_watermark());
    match self.call_plain(&RequestBody::Acknowledge { up_to })? {
      ReplyBody::Acknowledged => {
        self.acknowledged = self.acknowledged.max(up_to);
        Ok(())
      }
      _ => Err(ClientError::UnexpectedReply {
        verb: "acknowledge",
      }),
    }
  }

  /// The highest sequence acknowledged so far.
  pub fn acknowledged(&self) -> u32 {
    self.acknowledged
  }

  /// Acknowledges everything this client has received so far — short of an unpublished id awaiting
  /// its retry.
  pub fn acknowledge_all(&mut self) -> Result<(), ClientError> {
    let up_to = self.ack_watermark();
    self.acknowledge(up_to)
  }

  /// The base entry's bytes as the disk holds them now.
  pub fn read_base(&mut self, volume: VolumeId, path: &str) -> Result<Vec<u8>, ClientError> {
    match self.call(&RequestBody::ReadBase {
      volume,
      path: path.to_owned(),
    })? {
      ReplyBody::BaseBytes { bytes } => Ok(bytes),
      _ => Err(ClientError::UnexpectedReply { verb: "read_base" }),
    }
  }

  /// A clean base file's verified content digest (§4.15): the BLAKE3 of the bytes the disk holds
  /// for an untouched entry and their length, verified current at the export. Refused
  /// `DigestNotClean` for an entry the volume diverged (read and hash its bytes instead) and
  /// `DigestUnverified` when the file changed under the hash (retry).
  pub fn digest(&mut self, volume: VolumeId, path: &str) -> Result<Digest, ClientError> {
    match self.call(&RequestBody::Digest {
      volume,
      path: path.to_owned(),
    })? {
      ReplyBody::Digest { identity, size } => Ok(Digest { identity, size }),
      _ => Err(ClientError::UnexpectedReply { verb: "digest" }),
    }
  }

  /// Re-witnesses drifted entries (all of them when `paths` is none); the paths done.
  pub fn rewitness(
    &mut self,
    volume: VolumeId,
    paths: Option<Vec<String>>,
  ) -> Result<Vec<String>, ClientError> {
    match self.call(&RequestBody::Rewitness { volume, paths })? {
      ReplyBody::Rewitnessed { paths } => Ok(paths),
      _ => Err(ClientError::UnexpectedReply { verb: "rewitness" }),
    }
  }

  /// Pins base subtrees (the whole base when `paths` is none); entries pinned.
  pub fn pin(&mut self, volume: VolumeId, paths: Option<Vec<String>>) -> Result<u64, ClientError> {
    match self.call(&RequestBody::Pin { volume, paths })? {
      ReplyBody::Pinned { entries } => Ok(entries),
      _ => Err(ClientError::UnexpectedReply { verb: "pin" }),
    }
  }
}

/// A file verb's reply: the changed object's size, or the unexpected reply named by `verb`.
fn fs_done(reply: ReplyBody, verb: &'static str) -> Result<u64, ClientError> {
  match reply {
    ReplyBody::FsDone { size } => Ok(size),
    _ => Err(ClientError::UnexpectedReply { verb }),
  }
}
