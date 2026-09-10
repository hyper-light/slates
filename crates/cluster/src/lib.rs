//! The cluster plane (§4.8, D-14) — the **asynchronous dispatch** that ships a register record to its
//! candidate holders over the fleet transport and commits at a quorum. The ownership split (Ada's, the
//! cluster milestone): `slates-db` owns synchronous acceptance and the register protocol (the
//! [`Acceptor`], [`Placement`], [`Quorum`], the record and its identity); `slates-transport` carries
//! the mutually-authenticated request/reply; and this crate owns the dispatch, the collection, the
//! deadline (with progress-based extension, §4.8 "late work" — a commit whose quorum is still filling
//! is given more time rather than declared failed), and the cancellation. Both a local and a remote
//! holder run the *same* acceptance rules —
//! the owner holds locally through its own [`Acceptor`], each remote holder through one reached over
//! the transport.
//!
//! First milestone (this crate's scope): register commits **within one validated configuration
//! generation**. The caller supplies a validated configuration and owner-authority snapshot (the
//! [`Acceptor`]s' [`Authority`](slates_db::register::Authority) and the candidate set); SWIM/Lifeguard
//! membership, the configuration group, takeover and the mirror are the rest of the cluster plane and
//! are owed. The evidence the tests produce is "production endpoints and holder logic over simulated
//! UDP": one process may simulate several holders, but that is still an `f = 1` protocol topology;
//! real network/process deployment is a further gate.
//!
//! Correctness the dispatch preserves: a slow or dead holder never blocks quorum (each request runs in
//! its own task; the collection stops at `f + 1` distinct binding acknowledgements or at a deadline),
//! and a deadline reached after partial acceptance is reported as **uncertain** — it never claims the
//! write did not happen, and the record's identity is unchanged so a retry is idempotent at the holders
//! that already accepted (§4.8 "network receipt is not acceptance"; a retry preserves record identity).
//!
//! Membership ([`membership`]): the SWIM/Lifeguard failure-detection view that maintains which hosts
//! are alive — the neighbourhood the configuration and placement draw from — and the [`detector`]
//! protocol-period machine that probes members and drives that view alive → suspect → dead. The
//! configuration group ([`config_group`]) turns that view into the versioned
//! [`Configuration`](slates_db::register::Configuration) each request carries: it reconciles the
//! neighbourhood to the alive membership, advancing the version so a stale request is refused (the
//! `f = 0` degenerate of the fleet consensus, which is owed).

pub mod config_group;
pub mod coordinates;
pub mod detector;
pub(crate) mod fixed;
pub mod fleet;
pub mod membership;
pub mod progress;
pub mod raft;
pub mod raft_wire;
pub mod routing;
pub mod swim;

use std::sync::mpsc::{TryRecvError, channel};

use slates_db::ledger::{self, LedgerAcceptor, LedgerPromise};
use slates_db::register::{
  Accepted, Acceptor, Ack, Configuration, HostId, ObjectId, Placement, Prepare, Promise, Promotion,
  Quorum, Record, candidates_for,
};
use slates_rt::error::RtError;
use slates_rt::futures::{cancel, now_ns, sleep, spawn_child};
use slates_transport::endpoint::Endpoint;

use crate::progress::{DeadlineExtender, ExtensionOutcome, ProgressWitness};

/// The stream a record request rides on a holder connection.
/// Format: the register-ship RPC uses one stream per connection; the holder's `serve_once` accepts
/// whichever stream arrives, so the exact id is a fixed label, not a tunable.
const RECORD_STREAM: u64 = 1;

/// A refusal committing a record across the cluster.
#[derive(Debug)]
pub enum ClusterError {
  /// The runtime refused to spawn or drive a dispatch task.
  Runtime(RtError),
  /// Every holder responded but too few acknowledged to reach the quorum — the write is **not**
  /// placed, and (because every holder answered) it is known not to have committed.
  NotPlaced {
    /// What was acknowledged (fewer than `f + 1` distinct candidates).
    placement: Placement,
  },
  /// The deadline elapsed before a quorum acknowledged. The outcome is **uncertain**: some holders may
  /// have accepted and stored the record. The record's identity is unchanged, so a retry is idempotent
  /// at any holder that already accepted; it must not be treated as "nothing happened".
  Uncertain {
    /// What had been acknowledged when the deadline elapsed.
    placement: Placement,
  },
}

impl std::fmt::Display for ClusterError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ClusterError::Runtime(e) => write!(f, "cluster dispatch runtime error: {e:?}"),
      ClusterError::NotPlaced { placement } => {
        write!(
          f,
          "not placed: {} of the quorum acknowledged",
          placement.acked.len()
        )
      }
      ClusterError::Uncertain { placement } => write!(
        f,
        "uncertain: the deadline elapsed with {} acknowledgement(s); a retry is idempotent",
        placement.acked.len()
      ),
    }
  }
}

impl std::error::Error for ClusterError {}

impl From<RtError> for ClusterError {
  fn from(e: RtError) -> Self {
    ClusterError::Runtime(e)
  }
}

/// Serves one register record on a holder: receives the record over the transport, runs it through the
/// holder's [`Acceptor`] (which authorizes the owner and generation, fences the epoch, refuses a
/// conflicting committed value, and **stores** the accepted value before acknowledging), and replies
/// with the binding [`Ack`] — or an empty reply if the acceptor refuses, so the owner counts nothing.
/// The holder loops this for successive records; the `acceptor` is its persistent store.
pub async fn serve_record(
  endpoint: &mut Endpoint,
  acceptor: &mut Acceptor,
) -> Result<(), slates_transport::endpoint::EndpointError> {
  endpoint
    .serve_once(|request| match Record::decode(&request) {
      Ok(record) => match acceptor.accept(&record) {
        Ok(ack) => ack.encode(),
        Err(_) => Vec::new(),
      },
      Err(_) => Vec::new(),
    })
    .await
}

/// The timing budget for a commit's dispatch, the caller's to derive (owed — a measured RTT budget;
/// nothing here is a hidden constant): the deadline before an unfinished commit is reported uncertain,
/// the interval the collection loop parks between wake-ups, and the **progress-extension policy** (§4.8
/// "late work"). A commit whose quorum is still filling as it nears the deadline is granted a bounded
/// extension rather than declared uncertain; a stalled one (no new acknowledgement within the stall
/// window) is left to time out. The degenerate is [`hard`](CommitBudget::hard): no extension, so the
/// deadline is absolute — the behaviour every `f = 0` / laptop caller uses, the *same* code path a
/// fleet runs with extensions enabled (R8: the extender always runs, with a zero budget at the
/// degenerate).
#[derive(Clone, Copy, Debug)]
pub struct CommitBudget {
  /// How long to wait for a quorum before reporting the commit uncertain (the initial deadline, which a
  /// granted extension grows).
  pub deadline_ns: u64,
  /// The poll interval the collection loop sleeps between checking for acknowledgements.
  pub poll_interval_ns: u64,
  /// The fraction of the current deadline (as the integer ratio `numerator / denominator`, so no float
  /// enters the decision) at which an extension is first considered.
  lookahead_numerator: u64,
  lookahead_denominator: u64,
  /// How much each granted extension adds to the deadline.
  extension_ns: u64,
  /// How many extensions a single dispatch may be granted before its deadline is absolute.
  max_extensions: u32,
  /// The span of no new acknowledgement after which a dispatch is judged stalled, not merely slow.
  stall_window_ns: u64,
}

impl CommitBudget {
  /// A hard-deadline budget: the dispatch waits until `deadline_ns` and is then reported uncertain, with
  /// no progress extension. This is the degenerate — `max_extensions = 0`, so the extension policy
  /// reduces to the absolute deadline — that the laptop and every current caller use; the `f > 0` caller
  /// that wants late-work tolerance builds one with [`with_extension`](CommitBudget::with_extension).
  /// `poll_interval_ns` is the collection loop's park between wake-ups.
  pub fn hard(deadline_ns: u64, poll_interval_ns: u64) -> CommitBudget {
    CommitBudget {
      deadline_ns,
      poll_interval_ns,
      // No extension: the lookahead falls at the deadline itself (1/1) and zero grants are allowed, so
      // the extender returns Continue until `deadline_ns` and Expire at it — an absolute deadline.
      lookahead_numerator: 1,
      lookahead_denominator: 1,
      extension_ns: 0,
      max_extensions: 0,
      // Never consulted while `max_extensions` is 0 (no extension is ever considered); set to the
      // deadline so the field is never a smaller hidden bound than the deadline it accompanies.
      stall_window_ns: deadline_ns,
    }
  }

  /// A progress-extending budget (§4.8 "late work"): a dispatch still gathering acknowledgements as it
  /// passes the `lookahead_numerator / lookahead_denominator` fraction of its deadline is granted up to
  /// `max_extensions` extensions of `extension_ns` each, provided its acknowledged set advanced within
  /// `stall_window_ns`; a stalled dispatch is left to time out at the current deadline. Every value is
  /// the caller's to derive from the operation's class (its measured per-holder RTT and quorum width);
  /// nothing here is a hidden constant.
  pub fn with_extension(
    deadline_ns: u64,
    poll_interval_ns: u64,
    lookahead_numerator: u64,
    lookahead_denominator: u64,
    extension_ns: u64,
    max_extensions: u32,
    stall_window_ns: u64,
  ) -> CommitBudget {
    CommitBudget {
      deadline_ns,
      poll_interval_ns,
      lookahead_numerator,
      lookahead_denominator,
      extension_ns,
      max_extensions,
      stall_window_ns,
    }
  }

  /// The extension policy this budget describes, as a fresh [`DeadlineExtender`] seeded at `deadline_ns`.
  fn extender(&self) -> DeadlineExtender {
    DeadlineExtender::new(
      self.deadline_ns,
      self.lookahead_numerator,
      self.lookahead_denominator,
      self.extension_ns,
      self.max_extensions,
    )
  }

  /// The longest the collection loop can wait under this budget — the base deadline plus every extension it
  /// may grant. A dispatch task bounds its own wait ([`request_within`]) by this, so it hands its holder's
  /// session back no later than the loop stops waiting for it, and a reply arriving during a granted
  /// extension is still received rather than cut off at the base deadline. At the [`hard`](CommitBudget::hard)
  /// degenerate (no extensions) this is exactly the base deadline.
  fn max_deadline_ns(&self) -> u64 {
    self.deadline_ns.saturating_add(
      self
        .extension_ns
        .saturating_mul(u64::from(self.max_extensions)),
    )
  }
}

/// The dispatch collection loop's wait-and-decide step (§4.8 "late work"), shared by the commit
/// ([`collect_acks`]) and promotion ([`collect_promises`]) loops so both age a slow dispatch the same
/// way. It parks one poll interval, then judges from the dispatch's own progress — the size of its
/// acknowledged (or promised) set — whether a dispatch that has reached its deadline is still filling
/// its quorum (extend) or has stalled / spent its extension budget (time out). The [`hard`] budget's
/// zero extension budget makes this an absolute deadline, so the laptop's behaviour is unchanged (R8).
///
/// [`hard`]: CommitBudget::hard
struct DispatchWait {
  extender: DeadlineExtender,
  witness: ProgressWitness,
  poll_interval_ns: u64,
  started_ns: u64,
}

impl DispatchWait {
  /// A wait seeded from `budget`, its clock started at `now` (the moment collection begins, so the
  /// elapsed time the extender measures is time spent gathering acknowledgements).
  fn new(budget: CommitBudget, now: u64) -> DispatchWait {
    DispatchWait {
      extender: budget.extender(),
      witness: ProgressWitness::new(budget.stall_window_ns, now),
      poll_interval_ns: budget.poll_interval_ns,
      started_ns: now,
    }
  }

  /// Parks one poll interval, then reports whether the dispatch may keep waiting given how many distinct
  /// acknowledgements it has gathered so far (`gathered`). Returns `false` when the dispatch has reached
  /// its deadline and is stalled, or has spent its extension budget — the collection loop then stops and
  /// the commit is reported uncertain. A dispatch below its deadline, or still filling its quorum near
  /// it, keeps waiting.
  async fn keep_waiting(&mut self, gathered: usize) -> bool {
    sleep(self.poll_interval_ns).await;
    let now = now_ns();
    self
      .witness
      .observe(u64::try_from(gathered).unwrap_or(u64::MAX), now);
    let elapsed = now.saturating_sub(self.started_ns);
    !matches!(
      self.extender.evaluate(elapsed, &self.witness, now),
      ExtensionOutcome::Expire
    )
  }
}

/// What a dispatch task reports back to the owner's collection loop: a holder's reply bytes (empty if it
/// refused) *and its endpoint returned boxed* (an endpoint is large), so the connection is reused across
/// commits — its packet-number space stays continuous across a retry, RFC 9000 §12.3. The deadline is no
/// longer a message: it is the collection loop's own progress-extension decision ([`DispatchWait`]), so a
/// report is always a holder's reply.
struct Reply(HostId, Vec<u8>, Box<Endpoint>);

/// Runs one request/reply on `endpoint` (its `request`), racing it against `deadline_ns`, and returns the
/// reply bytes (empty on a timeout or a transport error) **together with the endpoint, kept whatever the
/// outcome**. This is what lets a dispatch task *always* hand its holder's session back to the collection
/// loop — a straggler that never replies still returns its endpoint at the deadline rather than blocking
/// until it is cancelled and its session dropped. A dropped session cannot be re-established in the
/// per-peer-socket mesh (`Endpoint::accept` pins one source), so keeping it is what lets the caller retry a
/// load-timed-out commit or promotion over the same warm session instead of stranding the object
/// (`docs/bugs/2026-09-10-swim-stale-ack.md` records the same discipline for the SWIM probe). The deadline
/// is the collection loop's own bound — its full progress-extended span
/// ([`CommitBudget::max_deadline_ns`]) — so a reply that arrives while the loop is still extending is not
/// cut off early.
async fn request_within(
  mut endpoint: Endpoint,
  stream_id: u64,
  request: &[u8],
  deadline_ns: u64,
) -> (Vec<u8>, Endpoint) {
  let reply = {
    let mut exchange = std::pin::pin!(endpoint.request(stream_id, request));
    let mut timer = std::pin::pin!(sleep(deadline_ns));
    std::future::poll_fn(|cx| {
      // Prefer a delivered reply over the deadline when both are ready.
      if let std::task::Poll::Ready(result) = std::future::Future::poll(exchange.as_mut(), cx) {
        return std::task::Poll::Ready(result.ok());
      }
      if std::future::Future::poll(timer.as_mut(), cx).is_ready() {
        return std::task::Poll::Ready(None);
      }
      std::task::Poll::Pending
    })
    .await
  };
  (reply.unwrap_or_default(), endpoint)
}

/// Whether `acked` (distinct candidates) commits under `quorum` for `candidates`.
fn is_placed(candidates: &[HostId], acked: &[HostId], quorum: Quorum) -> bool {
  Placement {
    candidates: candidates.to_vec(),
    acked: acked.to_vec(),
    mirror_acked: None,
  }
  .placed(quorum)
}

/// Collects replies until quorum or the deadline: records each distinct, binding acknowledgement into
/// `acked`, keeps every replying holder's endpoint for reuse, and returns the reusable endpoints and
/// whether the deadline was reached. Recovers the endpoints of tasks that finished after the loop.
async fn collect_acks(
  rx: std::sync::mpsc::Receiver<Reply>,
  record: &Record,
  candidates: &[HostId],
  quorum: Quorum,
  budget: CommitBudget,
  acked: &mut Vec<HostId>,
) -> (Vec<(HostId, Endpoint)>, bool) {
  let mut reusable: Vec<(HostId, Endpoint)> = Vec::new();
  let mut timed_out = false;
  let mut wait = DispatchWait::new(budget, now_ns());
  while !is_placed(candidates, acked, quorum) {
    match rx.try_recv() {
      Ok(Reply(host, reply, endpoint)) => {
        reusable.push((host, *endpoint));
        if let Ok(ack) = Ack::decode(&reply)
          && ack.holder == host
          && ack.binds(record)
          && candidates.contains(&host)
          && !acked.contains(&host)
        {
          acked.push(host);
        }
      }
      // Nothing to receive: park a poll interval and let the progress-extension policy decide whether a
      // dispatch at its deadline is still filling its quorum (keep waiting) or has stalled (time out).
      Err(TryRecvError::Empty) => {
        if !wait.keep_waiting(acked.len()).await {
          timed_out = true;
          break;
        }
      }
      Err(TryRecvError::Disconnected) => break,
    }
  }
  // Recover the endpoint of any task that already finished, so it too is reused.
  while let Ok(Reply(host, _, endpoint)) = rx.try_recv() {
    reusable.push((host, *endpoint));
  }
  (reusable, timed_out)
}

/// A commit's outcome and the holder connections still open for the next commit — a holder that
/// replied before the quorum or deadline hands its [`Endpoint`] back, so a retry reuses the same
/// connection (continuous packet numbers) rather than re-establishing; a holder whose task was
/// cancelled (too slow) is absent and is re-established on next use.
pub struct Committed {
  /// Whether the write placed (a [`Placement`]) or why not (a [`ClusterError`]).
  pub outcome: Result<Placement, ClusterError>,
  /// The holder connections still open, for reuse on the next commit.
  pub reusable: Vec<(HostId, Endpoint)>,
}

/// Commits `record` across its `candidates` (§4.8 "records are sent to all candidates; committed at
/// `f + 1`"). The owner (`owner`) holds it locally through `owner_acceptor`; the `remote_holders` (a
/// connected [`Endpoint`] per remaining candidate) are each shipped the record concurrently — one task
/// per holder, so a slow holder never serializes the others — and the loop collects **distinct,
/// binding** acknowledgements until the placement is `placed(quorum)` or the budget's deadline expires,
/// polling at the budget's interval between wake-ups. A commit whose quorum is still filling as it nears
/// the deadline is granted the budget's progress extension rather than declared uncertain (§4.8 "late
/// work"); a stalled one is left to time out. It returns a [`Committed`]: on quorum the [`Placement`];
/// on the deadline [`ClusterError::Uncertain`] (partial acceptance may have occurred); if every holder
/// answered short of quorum [`ClusterError::NotPlaced`] — together with the holder connections that
/// replied, handed back for reuse (so a retry reuses the same connection and its continuous
/// packet-number space). Remaining tasks are cancelled. The budget is the caller's to derive (owed — a
/// measured RTT budget); nothing here is a hidden constant.
pub async fn commit_record(
  owner: HostId,
  owner_acceptor: &mut Acceptor,
  candidates: &[HostId],
  record: &Record,
  quorum: Quorum,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> Committed {
  let mut acked: Vec<HostId> = Vec::new();
  // The owner's local hold — it is a candidate among the holders (§4.8 "the owner among them").
  if owner_acceptor.accept(record).is_ok() && candidates.contains(&owner) {
    acked.push(owner);
  }

  let build = |acked: &[HostId]| Placement {
    candidates: candidates.to_vec(),
    acked: acked.to_vec(),
    mirror_acked: None,
  };

  // A local hold may already commit (f = 0: one candidate, the owner) — then no dispatch is needed.
  if is_placed(candidates, &acked, quorum) {
    return Committed {
      outcome: Ok(build(&acked)),
      reusable: Vec::new(),
    };
  }

  // Dispatch each remote holder in its own task, reporting to one channel; the collection loop below
  // bounds the wait itself (its progress-extension policy), so no deadline task is needed. Children of
  // this task, so they are cancelled if this future is dropped (no orphans).
  let (tx, rx) = channel::<Reply>();
  let record_bytes = record.encode();
  let deadline_ns = budget.max_deadline_ns();
  let mut tasks = Vec::new();
  for (host, endpoint) in remote_holders {
    let tx = tx.clone();
    let bytes = record_bytes.clone();
    let spawned = spawn_child(async move {
      // Bounded by the collection loop's full span, and hands the endpoint back whatever the outcome, so a
      // straggler that never replies still returns its session rather than having it dropped when this task
      // is cancelled — the caller can then retry a load-timed-out commit over the same warm session.
      let (reply, endpoint) = request_within(endpoint, RECORD_STREAM, &bytes, deadline_ns).await;
      let _ = tx.send(Reply(host, reply, Box::new(endpoint)));
    });
    match spawned {
      Ok(task) => tasks.push(task),
      Err(e) => return spawn_failed(&tasks, e),
    }
  }
  drop(tx); // so the channel disconnects once every task has ended

  let (reusable, timed_out) =
    collect_acks(rx, record, candidates, quorum, budget, &mut acked).await;

  // Cancel whatever is still running — a straggler cut off *before* its own deadline (an early quorum at
  // f > 1) loses its session, which is unavoidable without waiting for it; but at f = 1 the single holder
  // always reports (there is no early quorum without it), so its session is kept for a retry.
  for task in &tasks {
    let _ = cancel(*task);
  }

  let placement = build(&acked);
  let outcome = if placement.placed(quorum) {
    Ok(placement)
  } else if timed_out {
    Err(ClusterError::Uncertain { placement })
  } else {
    Err(ClusterError::NotPlaced { placement })
  };
  Committed { outcome, reusable }
}

/// Commits `record` using a validated [`Configuration`] as the authority interface (§4.8 "epoch
/// allocation derives from configuration authority") — the caller consumes one validated snapshot
/// rather than loose parameters. The quorum, the candidate holders (rendezvous over the
/// configuration's neighbourhood for the record's object), and the owner all come from it; the
/// per-holder authority (generation = the configuration's version, and the owner) is what the
/// `owner_acceptor` and each remote holder's acceptor already enforce, so a record inconsistent with
/// the configuration fails acceptance and gathers no quorum. This is where SWIM/Lifeguard membership
/// and the configuration group (owed) will publish the [`Configuration`] this reads.
pub async fn commit_under_configuration(
  configuration: &Configuration,
  record: &Record,
  owner_acceptor: &mut Acceptor,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> Committed {
  let candidates = candidates_for(
    configuration.owner,
    &configuration.neighbourhood,
    record.object,
    configuration.quorum,
  );
  commit_record(
    configuration.owner,
    owner_acceptor,
    &candidates,
    record,
    configuration.quorum,
    remote_holders,
    budget,
  )
  .await
}

/// The [`Committed`] returned when a dispatch task could not be spawned: the runtime error, the tasks
/// already started cancelled, and no endpoints recoverable (they moved into the spawned futures).
fn spawn_failed(tasks: &[slates_rt::TaskId], error: RtError) -> Committed {
  for task in tasks {
    let _ = cancel(*task);
  }
  Committed {
    outcome: Err(ClusterError::Runtime(error)),
    reusable: Vec::new(),
  }
}

// ── Phase one: promotion over the transport (§4.8 "Promotion and takeover") ──────────────────────
//
// The symmetric counterpart of the commit driver above: when the configuration group has taken over a
// dead owner's object (bumping the host epoch, advancing the generation, assigning the object to the
// surviving candidate the rendezvous ranks first), the new owner must run phase one before it serves —
// a batched round that raises each holder's fence to the new epoch and adopts the newest record held
// under the old one, so nothing that ever committed is lost (Continuity) and a resumed stale owner can
// no longer reach quorum (StaleNeverCommits). The register protocol (`slates-db`) owns the synchronous
// per-holder step ([`Acceptor::prepare`]) and the adoption rule; this drives it asynchronously over the
// transport, the same ownership split and the same dispatch shape as the commit round. The caller
// supplies acceptors whose authority the configuration group already advanced (owner = the successor,
// generation = the new version) — the config distribution that precedes the takeover.

/// The stream a phase-one prepare request rides on a holder connection — distinct from the commit
/// stream so a holder can tell a promotion from a write.
/// Format: the register-promote RPC uses its own stream id per connection; a fixed label, not a tunable.
const PROMOTE_STREAM: u64 = 2;

/// Serves one phase-one prepare on a holder: receives the [`Prepare`] over the transport, runs it
/// through the holder's [`Acceptor`] (which checks the installed generation and owner, raises the fence
/// to the new epoch, and reports the highest record it holds for the object), and replies with the
/// binding [`Promise`] — or an empty reply if the acceptor refuses (a foreign generation, an
/// unauthorized owner, or an epoch below the fence), so the new owner counts nothing. The holder loops
/// this for successive prepares; the `acceptor` is its persistent store, shared with [`serve_record`].
pub async fn serve_promotion(
  endpoint: &mut Endpoint,
  acceptor: &mut Acceptor,
) -> Result<(), slates_transport::endpoint::EndpointError> {
  endpoint
    .serve_once(|request| match Prepare::decode(&request) {
      Ok(prepare) => match acceptor.prepare(&prepare) {
        Ok(promise) => promise.encode(),
        Err(_) => Vec::new(),
      },
      Err(_) => Vec::new(),
    })
    .await
}

/// A promotion's outcome and the holder connections still open for reuse — the phase-one counterpart of
/// [`Committed`]. On a quorum of promises the [`Promotion`] (the promising set and the adopted record);
/// on the deadline [`ClusterError::Uncertain`] with an empty placement (the promotion did not confirm);
/// short of quorum [`ClusterError::NotPlaced`]. The new owner completes the takeover by re-committing
/// [`Promotion::adoption_record`] with [`commit_record`] and only then serving.
pub struct Promoted {
  /// The promotion result: a quorum-backed [`Promotion`], or why it did not confirm.
  pub outcome: Result<Promotion, ClusterError>,
  /// The holder connections still open, for the adoption re-commit that follows.
  pub reusable: Vec<(HostId, Endpoint)>,
}

/// Collects promises until a quorum promised or the deadline: records each distinct, binding promise
/// into `promised`, folds the newest reported record into `adopted` (the same order the sans-io
/// [`promote_over_holders`] uses), keeps every replying holder's endpoint for reuse, and returns the
/// reusable endpoints and whether the deadline was reached. Recovers the endpoints of tasks that
/// finished after the loop.
async fn collect_promises(
  rx: std::sync::mpsc::Receiver<Reply>,
  prepare: &Prepare,
  candidates: &[HostId],
  quorum: Quorum,
  budget: CommitBudget,
  promised: &mut Vec<HostId>,
  adopted: &mut Option<Accepted>,
) -> (Vec<(HostId, Endpoint)>, bool) {
  let mut reusable: Vec<(HostId, Endpoint)> = Vec::new();
  let mut timed_out = false;
  let mut wait = DispatchWait::new(budget, now_ns());
  while !quorum.committed(promised.len()) {
    match rx.try_recv() {
      Ok(Reply(host, reply, endpoint)) => {
        reusable.push((host, *endpoint));
        if let Ok(promise) = Promise::decode(&reply)
          && promise.holder == host
          && promise.binds(prepare)
          && candidates.contains(&host)
          && !promised.contains(&host)
        {
          promised.push(host);
          fold_adopted(adopted, promise.highest);
        }
      }
      // Nothing to receive: park a poll interval and let the progress-extension policy decide whether a
      // promotion at its deadline is still filling its quorum (keep waiting) or has stalled (time out).
      Err(TryRecvError::Empty) => {
        if !wait.keep_waiting(promised.len()).await {
          timed_out = true;
          break;
        }
      }
      Err(TryRecvError::Disconnected) => break,
    }
  }
  while let Ok(Reply(host, _, endpoint)) = rx.try_recv() {
    reusable.push((host, *endpoint));
  }
  (reusable, timed_out)
}

/// Keeps the newer of the running adoption and a holder's reported record (§4.8 "adopts the newest
/// reported record per object"): a higher position, or the same position under a higher epoch, wins.
fn fold_adopted(adopted: &mut Option<Accepted>, reported: Option<Accepted>) {
  if let Some(reported) = reported {
    let keep = match adopted {
      Some(best) => reported.newer_than(best),
      None => true,
    };
    if keep {
      *adopted = Some(reported);
    }
  }
}

/// Runs phase one for `prepare` across its `candidates` (§4.8 "each new owner runs phase one in one
/// batched round"). The new owner (`new_owner`) promises locally through `owner_acceptor` (it is a
/// candidate — the surviving holder the takeover named); the `remote_holders` are each sent the prepare
/// concurrently — one task per holder, so a slow holder never serializes the others — and the loop
/// collects **distinct, binding** promises until a quorum promised or the budget's deadline expires (a
/// promotion still filling its quorum near the deadline earns the budget's progress extension, the same
/// §4.8 "late work" policy the commit uses), folding the newest adopted record as it goes. It returns a
/// [`Promoted`]: on a quorum the [`Promotion`] (safe to
/// serve — `f + 1` promises intersect every prior `f + 1` commit, so the adoption covers the committed
/// prefix); on the deadline [`ClusterError::Uncertain`]; short of quorum [`ClusterError::NotPlaced`] —
/// with the replying holders' connections handed back for the adoption re-commit. Remaining tasks are
/// cancelled. The budget is the caller's to derive (owed — a measured RTT budget); nothing here is a
/// hidden constant.
pub async fn promote_record(
  new_owner: HostId,
  owner_acceptor: &mut Acceptor,
  candidates: &[HostId],
  prepare: &Prepare,
  quorum: Quorum,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> Promoted {
  let mut promised: Vec<HostId> = Vec::new();
  let mut adopted: Option<Accepted> = None;
  // The new owner's own hold: it promises to itself, raising its fence and reporting its highest record.
  if let Ok(local) = owner_acceptor.prepare(prepare)
    && candidates.contains(&new_owner)
  {
    promised.push(new_owner);
    fold_adopted(&mut adopted, local.highest);
  }

  // A local promise may already be a quorum (f = 0: one candidate, the new owner) — no dispatch needed.
  if quorum.committed(promised.len()) {
    return Promoted {
      outcome: Ok(Promotion { promised, adopted }),
      reusable: Vec::new(),
    };
  }

  // Dispatch each remote holder in its own task, reporting to one channel; the collection loop below
  // bounds the wait itself (its progress-extension policy), so no deadline task is needed. Children of
  // this task, so they are cancelled if this future is dropped (no orphans).
  let (tx, rx) = channel::<Reply>();
  let prepare_bytes = prepare.encode();
  let deadline_ns = budget.max_deadline_ns();
  let mut tasks = Vec::new();
  for (host, endpoint) in remote_holders {
    let tx = tx.clone();
    let bytes = prepare_bytes.clone();
    let spawned = spawn_child(async move {
      // Bounded and endpoint-preserving (see `request_within`): a holder that does not promise in time still
      // returns its session, so the new owner can retry the takeover over the same warm session.
      let (reply, endpoint) = request_within(endpoint, PROMOTE_STREAM, &bytes, deadline_ns).await;
      let _ = tx.send(Reply(host, reply, Box::new(endpoint)));
    });
    match spawned {
      Ok(task) => tasks.push(task),
      Err(e) => return promote_spawn_failed(&tasks, e),
    }
  }
  drop(tx); // so the channel disconnects once every task has ended

  let (reusable, timed_out) = collect_promises(
    rx,
    prepare,
    candidates,
    quorum,
    budget,
    &mut promised,
    &mut adopted,
  )
  .await;

  // Cancel whatever is still running — a straggler's endpoint is dropped and that holder reconnects.
  for task in &tasks {
    let _ = cancel(*task);
  }

  let promotion = Promotion {
    promised: promised.clone(),
    adopted,
  };
  let outcome = if quorum.committed(promised.len()) {
    Ok(promotion)
  } else if timed_out {
    Err(ClusterError::Uncertain {
      placement: Placement {
        candidates: candidates.to_vec(),
        acked: promised,
        mirror_acked: None,
      },
    })
  } else {
    Err(ClusterError::NotPlaced {
      placement: Placement {
        candidates: candidates.to_vec(),
        acked: promised,
        mirror_acked: None,
      },
    })
  };
  Promoted { outcome, reusable }
}

/// The [`Promoted`] returned when a dispatch task could not be spawned: the runtime error, the tasks
/// already started cancelled, and no endpoints recoverable (they moved into the spawned futures).
fn promote_spawn_failed(tasks: &[slates_rt::TaskId], error: RtError) -> Promoted {
  for task in tasks {
    let _ = cancel(*task);
  }
  Promoted {
    outcome: Err(ClusterError::Runtime(error)),
    reusable: Vec::new(),
  }
}

/// Runs phase one using a validated post-takeover [`Configuration`] as the authority interface (§4.8
/// "epoch allocation derives from configuration authority") — the counterpart of
/// [`commit_under_configuration`]. The new owner, the bumped epoch, the generation and the candidate
/// holders (rendezvous over the configuration's neighbourhood for `object`) all come from it, so the
/// prepare is consistent with the configuration the holders have installed. This is where the
/// configuration group (owed) publishes the taken-over [`Configuration`] this reads.
pub async fn promote_under_configuration(
  configuration: &Configuration,
  object: ObjectId,
  owner_acceptor: &mut Acceptor,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> Promoted {
  let prepare = Prepare {
    owner: configuration.owner,
    object,
    epoch: configuration.host_epoch,
    generation: configuration.version,
  };
  let candidates = candidates_for(
    configuration.owner,
    &configuration.neighbourhood,
    object,
    configuration.quorum,
  );
  promote_record(
    configuration.owner,
    owner_acceptor,
    &candidates,
    &prepare,
    configuration.quorum,
    remote_holders,
    budget,
  )
  .await
}

/// The stream a *ledger* phase-one prepare rides on a holder connection — distinct from the single-value
/// promotion's [`PROMOTE_STREAM`] so a holder serving both tells them apart.
/// Format: one stream id per RPC kind on a connection; the holder's serve accepts whichever arrives.
const LEDGER_PROMOTE_STREAM: u64 = 3;

/// A confirmed ledger promotion: the holders that promised, and the log the new owner adopts — per
/// position, the identity carried under the highest epoch across the phase-one quorum ([`ledger::adopt`]).
/// The committed prefix of this log is every record that had committed under the old owner (Continuity),
/// because the phase-one quorum intersects every prior `f + 1` commit quorum.
pub struct LedgerPromotion {
  /// The candidates that returned a binding promise (a phase-one quorum).
  pub promised: Vec<HostId>,
  /// The adopted log, dense by position — the committed prefix and any adopted uncommitted tail.
  pub adopted: Vec<[u8; 32]>,
}

/// A ledger promotion's outcome and the holder connections still open for reuse — the multi-entry
/// counterpart of [`Promoted`]. On a quorum of promises the [`LedgerPromotion`] (the adopted log and the
/// promising set); on the deadline [`ClusterError::Uncertain`]; short of quorum [`ClusterError::NotPlaced`].
/// The new owner completes the takeover by re-committing the adopted log and only then serving.
pub struct LedgerPromoted {
  /// The promotion result: the adopted log and promising set, or why it did not confirm.
  pub outcome: Result<LedgerPromotion, ClusterError>,
  /// The holder connections still open, for the adoption re-commit that follows.
  pub reusable: Vec<(HostId, Endpoint)>,
}

/// The holder side of ledger phase one: serve one [`Prepare`], replying with this holder's whole log (a
/// [`LedgerPromise`]) after raising its fence. A prepare the holder refuses (foreign generation,
/// unauthorised owner, stale epoch) or that does not decode replies with empty bytes, which the new owner
/// does not count toward its quorum. The `acceptor` is the holder's persistent ledger store for the object.
pub async fn serve_ledger_promotion(
  endpoint: &mut Endpoint,
  acceptor: &mut LedgerAcceptor,
) -> Result<(), slates_transport::endpoint::EndpointError> {
  endpoint
    .serve_once(|request| match Prepare::decode(&request) {
      Ok(prepare) => match acceptor.prepare(&prepare) {
        Ok(promise) => promise.encode(),
        Err(_) => Vec::new(),
      },
      Err(_) => Vec::new(),
    })
    .await
}

/// Collects ledger promises until a phase-one quorum promised or the deadline: records each distinct,
/// binding promise's holder into `promised` and its whole log into `logs` (the set the new owner adopts
/// across), keeps every replying holder's endpoint for reuse, and returns the reusable endpoints and
/// whether the deadline was reached. The same progress-extension policy the commit and single-value
/// promotion use bounds the wait.
async fn collect_ledger_promises(
  rx: std::sync::mpsc::Receiver<Reply>,
  prepare: &Prepare,
  candidates: &[HostId],
  quorum: Quorum,
  budget: CommitBudget,
  promised: &mut Vec<HostId>,
  logs: &mut Vec<Vec<ledger::Record>>,
) -> (Vec<(HostId, Endpoint)>, bool) {
  let mut reusable: Vec<(HostId, Endpoint)> = Vec::new();
  let mut timed_out = false;
  let mut wait = DispatchWait::new(budget, now_ns());
  while !quorum.committed(promised.len()) {
    match rx.try_recv() {
      Ok(Reply(host, reply, endpoint)) => {
        reusable.push((host, *endpoint));
        if let Ok(promise) = LedgerPromise::decode(&reply)
          && promise.holder == host
          && promise.binds(prepare)
          && candidates.contains(&host)
          && !promised.contains(&host)
        {
          promised.push(host);
          logs.push(promise.log);
        }
      }
      // Nothing to receive: park a poll interval and let the progress-extension policy decide whether a
      // promotion at its deadline is still filling its quorum (keep waiting) or has stalled (time out).
      Err(TryRecvError::Empty) => {
        if !wait.keep_waiting(promised.len()).await {
          timed_out = true;
          break;
        }
      }
      Err(TryRecvError::Disconnected) => break,
    }
  }
  while let Ok(Reply(host, _, endpoint)) = rx.try_recv() {
    reusable.push((host, *endpoint));
  }
  (reusable, timed_out)
}

/// Runs ledger phase one for `prepare` across its `candidates` (§4.8 "each new owner runs phase one in one
/// batched round"), the multi-entry counterpart of [`promote_record`]. The new owner (`new_owner`)
/// promises locally through `owner_acceptor` (it is a candidate — the surviving holder the takeover
/// named); the `remote_holders` are each sent the prepare concurrently — one task per holder — and the
/// loop collects distinct, binding promises, each a holder's whole log, until a quorum promised or the
/// budget's deadline expires (the same §4.8 "late work" progress extension the commit uses). On a quorum
/// it adopts, per position, the record under the highest epoch across the promising quorum
/// ([`ledger::adopt`]); the adopted log's committed prefix is every record that had committed under the
/// old owner (Continuity), because a phase-one quorum and every prior `f + 1` commit quorum both intersect
/// at `f + 1` of `2f + 1`. It returns a [`LedgerPromoted`]: on a quorum the adopted log (safe to re-commit
/// and serve); on the deadline [`ClusterError::Uncertain`]; short of quorum [`ClusterError::NotPlaced`] —
/// with the replying holders' connections for the adoption re-commit. Remaining tasks are cancelled. The
/// budget is the caller's to derive (owed — a measured RTT budget); nothing here is a hidden constant.
pub async fn promote_ledger_record(
  new_owner: HostId,
  owner_acceptor: &mut LedgerAcceptor,
  candidates: &[HostId],
  prepare: &Prepare,
  quorum: Quorum,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> LedgerPromoted {
  let mut promised: Vec<HostId> = Vec::new();
  let mut logs: Vec<Vec<ledger::Record>> = Vec::new();
  // The new owner's own hold: it promises to itself, raising its fence and reporting its whole log.
  if let Ok(local) = owner_acceptor.prepare(prepare)
    && candidates.contains(&new_owner)
  {
    promised.push(new_owner);
    logs.push(local.log);
  }

  // A local promise may already be a quorum (f = 0: one candidate, the new owner) — no dispatch needed.
  if quorum.committed(promised.len()) {
    return LedgerPromoted {
      outcome: Ok(LedgerPromotion {
        promised,
        adopted: ledger::adopt(&logs),
      }),
      reusable: Vec::new(),
    };
  }

  // Dispatch each remote holder in its own task, reporting to one channel; the collection loop bounds the
  // wait itself (its progress-extension policy). Children of this task, so they are cancelled if this
  // future is dropped (no orphans).
  let (tx, rx) = channel::<Reply>();
  let prepare_bytes = prepare.encode();
  let deadline_ns = budget.max_deadline_ns();
  let mut tasks = Vec::new();
  for (host, endpoint) in remote_holders {
    let tx = tx.clone();
    let bytes = prepare_bytes.clone();
    let spawned = spawn_child(async move {
      // Bounded and endpoint-preserving (see `request_within`), so a holder's session survives a timeout.
      let (reply, endpoint) =
        request_within(endpoint, LEDGER_PROMOTE_STREAM, &bytes, deadline_ns).await;
      let _ = tx.send(Reply(host, reply, Box::new(endpoint)));
    });
    match spawned {
      Ok(task) => tasks.push(task),
      Err(e) => return ledger_promote_spawn_failed(&tasks, e),
    }
  }
  drop(tx); // so the channel disconnects once every task has ended

  let (reusable, timed_out) = collect_ledger_promises(
    rx,
    prepare,
    candidates,
    quorum,
    budget,
    &mut promised,
    &mut logs,
  )
  .await;

  // Cancel whatever is still running — a straggler's endpoint is dropped and that holder reconnects.
  for task in &tasks {
    let _ = cancel(*task);
  }

  let outcome = if quorum.committed(promised.len()) {
    Ok(LedgerPromotion {
      promised,
      adopted: ledger::adopt(&logs),
    })
  } else if timed_out {
    Err(ClusterError::Uncertain {
      placement: Placement {
        candidates: candidates.to_vec(),
        acked: promised,
        mirror_acked: None,
      },
    })
  } else {
    Err(ClusterError::NotPlaced {
      placement: Placement {
        candidates: candidates.to_vec(),
        acked: promised,
        mirror_acked: None,
      },
    })
  };
  LedgerPromoted { outcome, reusable }
}

/// The [`LedgerPromoted`] returned when a dispatch task could not be spawned: the runtime error, the tasks
/// already started cancelled, and no endpoints recoverable (they moved into the spawned futures).
fn ledger_promote_spawn_failed(tasks: &[slates_rt::TaskId], error: RtError) -> LedgerPromoted {
  for task in tasks {
    let _ = cancel(*task);
  }
  LedgerPromoted {
    outcome: Err(ClusterError::Runtime(error)),
    reusable: Vec::new(),
  }
}
