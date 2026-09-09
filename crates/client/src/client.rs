//! The client: the rendezvous, one request in flight, exactly-once retries, typed verbs.

#[cfg(unix)]
use std::os::fd::RawFd;
use std::time::Instant;

use slates_ipc::protocol::{
  AuditEntry, DaemonReport, Direction, Filter, GrantSummary, Intent, LandingOutcome,
  LandingSummary, MergeWindow, NamePolicy, ReplyBody, RequestBody, Scope, SizeClass, SnapshotId,
  StatusReport, VolumeId, VolumeSummary, WorkOp, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect_as};
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

/// What `attach` returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
  /// The attachment id (what `detach` takes).
  pub attachment: u64,
  /// The lease epoch, for a write attachment.
  pub lease_epoch: Option<u64>,
  /// The path a bridge publishes (none until a bridge exists).
  pub path: Option<String>,
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
  /// Replies drained from the completion ring while looking for another request's reply, held by
  /// their request-id word until their own [`Client::poll_reply`] takes them. The async path drains
  /// the ring on a completion-fd signal and matches by id, so replies that arrive out of order (a
  /// deferred verb) or unawaited (an acknowledgement) do not block another request's. Bounded — an
  /// overflow drops the oldest, which can only be an unawaited reply, never one still in flight.
  pending: Vec<(u64, ReplyBody)>,
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
      .finish()
  }
}

/// Derived: the first pause between reconnect attempts is the wake latency the daemon
/// published as its spin window (the shortest wait that is not a spin); each pause doubles
/// up to a tenth of the budget, so a restart is seen within a tenth of the budget at worst
/// and a dead daemon costs no core.
fn next_pause_ns(pause_ns: u64, budget_ns: u64) -> u64 {
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
    ReplyBody::Snapshotted { id } => Ok(id),
    _ => Err(ClientError::UnexpectedReply { verb: "snapshot" }),
  }
}

/// Extracts a status report from its reply, or a typed mismatch.
fn extract_status(body: ReplyBody) -> Result<StatusReport, ClientError> {
  match body {
    ReplyBody::Status { report } => Ok(report),
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

impl Client {
  /// Connects to `instance` as a new client.
  pub fn connect(instance: &str, deadlines: Deadlines) -> Result<Client, ClientError> {
    let connected = connect_as(instance, 0)?;
    let client_id = connected.region.client_id();
    let end = ClientEnd::connected(connected);
    let ack_every = ack_every_of(&end);
    Ok(Client {
      instance: instance.to_owned(),
      end,
      client_id,
      sequence: 0,
      deadlines,
      reconnects: 0,
      acknowledged: 0,
      ack_every,
      pending: Vec::new(),
    })
  }

  /// Resumes a session: connects under its client id (refused `SessionTaken` when a live
  /// client holds it) and continues its sequence, so a retry of what it had in flight meets
  /// the completion record (§4.9).
  pub fn resume(
    instance: &str,
    session: Session,
    deadlines: Deadlines,
  ) -> Result<Client, ClientError> {
    let connected = connect_as(instance, session.client_id)?;
    let assigned = connected.region.client_id();
    if assigned != session.client_id {
      return Err(ClientError::SessionTaken { assigned });
    }
    let end = ClientEnd::connected(connected);
    let ack_every = ack_every_of(&end);
    Ok(Client {
      instance: instance.to_owned(),
      end,
      client_id: session.client_id,
      sequence: session.next_sequence.saturating_sub(1),
      deadlines,
      reconnects: 0,
      acknowledged: 0,
      ack_every,
      pending: Vec::new(),
    })
  }

  /// The client id the daemon bound to this principal.
  pub fn client_id(&self) -> u32 {
    self.client_id
  }

  /// The session to resume later.
  pub fn session(&self) -> Session {
    Session {
      client_id: self.client_id,
      next_sequence: self.sequence.wrapping_add(1),
    }
  }

  /// The last request id used (what a retry after a restart would carry).
  pub fn last_request(&self) -> RequestId {
    RequestId {
      client: self.client_id,
      sequence: self.sequence,
    }
  }

  /// Spins this long for a reply before parking (`None`: the daemon's published window, the
  /// measured wake cost; the 2-competitive choice for CPU). A caller with a latency floor of
  /// its own spins for it and never pays a wake when the daemon meets it.
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

  /// Sends `body` as the next request and returns the reply body, refusals typed.
  pub fn call(&mut self, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    // Every `ack_every` replies, the client acknowledges them first (one request), so the
    // daemon's retained records stay bounded without the caller's help (§4.9).
    if self.sequence.wrapping_sub(self.acknowledged) >= self.ack_every {
      let up_to = self.sequence;
      self.acknowledge(up_to)?;
    }
    self.call_plain(body)
  }

  /// One request under the next sequence, without the automatic acknowledgement.
  fn call_plain(&mut self, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    self.sequence = self.sequence.wrapping_add(1);
    let id = RequestId {
      client: self.client_id,
      sequence: self.sequence,
    };
    self.exchange(id, body)
  }

  /// Sends `body` under `id` again (a retry: the daemon answers from its completion record
  /// when it served the id before, else serves it now).
  pub fn retry(&mut self, id: RequestId, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    self.exchange(id, body)
  }

  /// One request, one reply; a stalled reply from a gone daemon becomes a reconnect and a
  /// resend under the same id.
  fn exchange(&mut self, id: RequestId, body: &RequestBody) -> Result<ReplyBody, ClientError> {
    loop {
      self.send(id, body)?;
      match self.end.wait(Some(self.deadlines.reply_ns)) {
        Ok(reply) => {
          if reply.request != id.word() {
            return Err(ClientError::UnexpectedReply {
              verb: "another request's reply",
            });
          }
          let decoded: ReplyBody = unpack(self.end.region(), reply.kind, &reply.payload)?;
          return match decoded {
            ReplyBody::Refused { refusal } => Err(ClientError::Refused(refusal)),
            other => Ok(other),
          };
        }
        Err(IpcError::DeadlineExceeded) => {
          if self.end.daemon_gone() {
            self.reconnect()?;
          } else {
            return Err(ClientError::Stalled {
              after_ns: self.deadlines.reply_ns,
            });
          }
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
    self.sequence = self.sequence.wrapping_add(1);
    let id = RequestId {
      client: self.client_id,
      sequence: self.sequence,
    };
    self.send(id, body)?;
    Ok(id)
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
      return resolved(body).map(Some);
    }
    while let Some(reply) = self.end.try_take()? {
      let body: ReplyBody = unpack(self.end.region(), reply.kind, &reply.payload)?;
      if reply.request == word {
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

  /// Buffers a reply drained for another request. Bounded (item 8): no more requests can be in flight
  /// than the ring holds, so an awaited reply is always within one ring's worth; twice that as the cap
  /// means an overflow drops only an unawaited reply (a periodic acknowledgement), never one still
  /// awaited.
  fn buffer(&mut self, id_word: u64, body: ReplyBody) {
    self.pending.push((id_word, body));
    let bound = self.reply_buffer_bound();
    while self.pending.len() > bound {
      self.pending.remove(0);
    }
  }

  /// Derived: the reply buffer's bound, twice the command ring's slots (an awaited reply is always
  /// within one ring of in-flight requests, so twice that never evicts one still awaited).
  fn reply_buffer_bound(&self) -> usize {
    derived!(
      self.end.region().cmd().slots().saturating_mul(2).max(1),
      "2 × region.slots",
      ["region.slots"]
    )
    .get()
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

  /// Sends the periodic acknowledgement if it is due, without waiting for its reply — the async
  /// caller drains and drops the `Acknowledged` reply as an id it never awaited. This keeps the
  /// daemon's retained completion records bounded (§4.9) the way [`Self::call`] does inline on the
  /// sync path, without a blocking round trip on the event loop. The acknowledged mark advances
  /// optimistically; a lost ack only means the daemon holds a little more until the next one.
  pub fn begin_ack_if_due(&mut self) -> Result<(), ClientError> {
    if self.sequence.wrapping_sub(self.acknowledged) >= self.ack_every {
      let up_to = self.sequence;
      self.begin(&RequestBody::Acknowledge { up_to })?;
      self.acknowledged = self.acknowledged.max(up_to);
    }
    Ok(())
  }

  /// The spin window the daemon published (nanoseconds): the async fast path spins this long taking
  /// the reply before it arms and yields to its event loop.
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

  /// Begins a create-green, returning its request id.
  pub fn create_green_begin(
    &mut self,
    name: &str,
    require_evidence: bool,
  ) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::CreateGreen {
      name: name.to_owned(),
      require_evidence,
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
    self.begin(&RequestBody::Edit {
      work,
      path: path.to_owned(),
      at,
      delete_len,
      bytes: bytes.to_vec(),
    })
  }

  /// Takes an edit's reply within `spin_ns` (`true` when done).
  pub fn edit_spin(&mut self, id: RequestId, spin_ns: u64) -> Result<Option<bool>, ClientError> {
    self.spin_as(id, spin_ns, extract_edited)
  }

  /// Takes an edit's reply by id word once the completion fd signals.
  pub fn edit_poll(&mut self, word: u64) -> Result<Option<bool>, ClientError> {
    self.poll_as(word, extract_edited)
  }

  /// Begins a submit of a work volume, returning its request id.
  pub fn submit_begin(&mut self, work: VolumeId) -> Result<RequestId, ClientError> {
    self.begin(&RequestBody::Submit { work })
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
  /// daemon, handled as a stalled reply is).
  fn send(&mut self, id: RequestId, body: &RequestBody) -> Result<(), ClientError> {
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
        Ok(()) => return Ok(()),
        Err(IpcError::RingFull) => {
          if elapsed_ns(started) < self.deadlines.reply_ns {
            std::hint::spin_loop();
            continue;
          }
          if self.end.daemon_gone() {
            self.reconnect()?;
          } else {
            return Err(ClientError::Stalled {
              after_ns: self.deadlines.reply_ns,
            });
          }
        }
        Err(e) => return Err(ClientError::Ipc(e)),
      }
    }
  }

  /// Reconnects under the client's id inside the reconnect budget; refused when the budget
  /// passes or a live client holds the id.
  fn reconnect(&mut self) -> Result<(), ClientError> {
    let started = Instant::now();
    let budget = self.deadlines.reconnect_ns;
    let mut pause_ns = u64::from(self.end.region().spin_ns()).max(1);
    loop {
      match connect_as(&self.instance, self.client_id) {
        Ok(connected) => {
          let assigned = connected.region.client_id();
          if assigned != self.client_id {
            return Err(ClientError::SessionTaken { assigned });
          }
          self.end = ClientEnd::connected(connected);
          self.reconnects += 1;
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
    match self.call(&RequestBody::Snapshot { volume })? {
      ReplyBody::Snapshotted { id } => Ok(id),
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

  /// Creates a green volume — a shared merge target (§4.16); its id.
  pub fn create_green(
    &mut self,
    name: &str,
    require_evidence: bool,
  ) -> Result<VolumeId, ClientError> {
    match self.call(&RequestBody::CreateGreen {
      name: name.to_owned(),
      require_evidence,
    })? {
      ReplyBody::GreenCreated { id } => Ok(id),
      _ => Err(ClientError::UnexpectedReply {
        verb: "create_green",
      }),
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

  /// Declares an edit on a work volume (§4.16): a splice at `path` — remove `delete_len` bytes at
  /// `at`, insert `bytes`.
  pub fn edit(
    &mut self,
    work: VolumeId,
    path: &str,
    at: u64,
    delete_len: u64,
    bytes: &[u8],
  ) -> Result<(), ClientError> {
    match self.call(&RequestBody::Edit {
      work,
      path: path.to_owned(),
      at,
      delete_len,
      bytes: bytes.to_vec(),
    })? {
      ReplyBody::Edited => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "edit" }),
    }
  }

  /// Declares a namespace or metadata operation on a work volume (§4.16): the counterpart to `edit`'s
  /// content splice — an unlink, rename, directory, mode, symlink, hard link or extended attribute.
  pub fn declare(&mut self, work: VolumeId, op: WorkOp) -> Result<(), ClientError> {
    match self.call(&RequestBody::Declare { work, op })? {
      ReplyBody::Declared => Ok(()),
      _ => Err(ClientError::UnexpectedReply { verb: "declare" }),
    }
  }

  /// Submits a work volume's declared operations to its green (§4.16): accepted at a new version, or
  /// a conflict with windows to rebase.
  pub fn submit(&mut self, work: VolumeId) -> Result<Submitted, ClientError> {
    match self.call(&RequestBody::Submit { work })? {
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

  /// Attaches (the client form; a write intent takes the lease).
  pub fn attach(
    &mut self,
    volume: VolumeId,
    snapshot: Option<SnapshotId>,
    intent: Intent,
  ) -> Result<Attachment, ClientError> {
    match self.call(&RequestBody::Attach {
      volume,
      snapshot,
      intent,
    })? {
      ReplyBody::Attached {
        attachment,
        lease_epoch,
        path,
      } => Ok(Attachment {
        attachment,
        lease_epoch,
        path,
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
      ReplyBody::Status { report } => Ok(report),
      _ => Err(ClientError::UnexpectedReply { verb: "status" }),
    }
  }

  /// The daemon's status (§4.14 `slates.status`).
  pub fn daemon_status(&mut self) -> Result<DaemonReport, ClientError> {
    match self.call(&RequestBody::DaemonStatus)? {
      ReplyBody::DaemonStatus { report } => Ok(report),
      _ => Err(ClientError::UnexpectedReply {
        verb: "daemon_status",
      }),
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

  /// Acknowledges every completion up to `up_to` (releases their records).
  pub fn acknowledge(&mut self, up_to: u32) -> Result<(), ClientError> {
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

  /// Acknowledges everything this client has received so far.
  pub fn acknowledge_all(&mut self) -> Result<(), ClientError> {
    let up_to = self.sequence;
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
