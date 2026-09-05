//! The client: the rendezvous, one request in flight, exactly-once retries, typed verbs.

use std::time::Instant;

use slates_ipc::protocol::{
  AuditEntry, DaemonReport, Direction, Filter, GrantSummary, Intent, LandingOutcome,
  LandingSummary, NamePolicy, ReplyBody, RequestBody, Scope, SizeClass, SnapshotId, StatusReport,
  VolumeId, VolumeSummary, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect_as};
use slates_machine::{Derived, derived};
use slates_wire::request::RequestId;

use crate::error::ClientError;

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
