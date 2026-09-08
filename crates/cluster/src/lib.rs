//! The cluster plane (§4.8, D-14) — the **asynchronous dispatch** that ships a register record to its
//! candidate holders over the fleet transport and commits at a quorum. The ownership split (Ada's, the
//! cluster milestone): `slates-db` owns synchronous acceptance and the register protocol (the
//! [`Acceptor`], [`Placement`], [`Quorum`], the record and its identity); `slates-transport` carries
//! the mutually-authenticated request/reply; and this crate owns the dispatch, the collection, the
//! deadline and the cancellation. Both a local and a remote holder run the *same* acceptance rules —
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

use std::sync::mpsc::{TryRecvError, channel};

use slates_db::register::{Acceptor, Ack, HostId, Placement, Quorum, Record};
use slates_rt::error::RtError;
use slates_rt::futures::{cancel, sleep, spawn_child};
use slates_transport::endpoint::Endpoint;

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

/// The timing budget for a commit's dispatch, both the caller's to derive (owed — a measured RTT
/// budget; nothing here is a hidden constant): the deadline before an unfinished commit is reported
/// uncertain, and the interval the collection loop parks between wake-ups.
#[derive(Clone, Copy, Debug)]
pub struct CommitBudget {
  /// How long to wait for a quorum before reporting the commit uncertain.
  pub deadline_ns: u64,
  /// The poll interval the collection loop sleeps between checking for acknowledgements.
  pub poll_interval_ns: u64,
}

/// What a dispatch task reports back to the owner's collection loop.
enum Reply {
  /// A holder's reply bytes (empty if it refused; decoded and bound-checked by the owner).
  From(HostId, Vec<u8>),
  /// The commit deadline elapsed.
  Deadline,
}

/// Commits `record` across its `candidates` (§4.8 "records are sent to all candidates; committed at
/// `f + 1`"). The owner (`owner`) holds it locally through `owner_acceptor`; the `remote_holders` (a
/// connected [`Endpoint`] per remaining candidate) are each shipped the record concurrently — one task
/// per holder, so a slow holder never serializes the others — and the loop collects **distinct,
/// binding** acknowledgements until the placement is `placed(quorum)` or `deadline_ns` elapses,
/// polling at `poll_interval_ns` between wake-ups. On quorum it returns the [`Placement`]; on the
/// deadline it returns [`ClusterError::Uncertain`] (partial acceptance may have occurred); if every
/// holder answered without reaching quorum it returns [`ClusterError::NotPlaced`]. Remaining tasks are
/// cancelled before returning. Both deadlines are the caller's to derive (owed — a measured RTT
/// budget); nothing here is a hidden constant.
pub async fn commit_record(
  owner: HostId,
  owner_acceptor: &mut Acceptor,
  candidates: &[HostId],
  record: &Record,
  quorum: Quorum,
  remote_holders: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> Result<Placement, ClusterError> {
  let mut acked: Vec<HostId> = Vec::new();
  // The owner's local hold — it is a candidate among the holders (§4.8 "the owner among them").
  if owner_acceptor.accept(record).is_ok() && candidates.contains(&owner) {
    acked.push(owner);
  }

  let placement = |acked: &[HostId]| Placement {
    candidates: candidates.to_vec(),
    acked: acked.to_vec(),
    mirror_acked: None,
  };

  // A local hold may already commit (f = 0: one candidate, the owner) — then no dispatch is needed.
  if placement(&acked).placed(quorum) {
    return Ok(placement(&acked));
  }

  // Dispatch each remote holder in its own task, reporting to one channel; a deadline task closes the
  // wait. Children of this task, so they are cancelled if this future is dropped (no orphans).
  let (tx, rx) = channel::<Reply>();
  let record_bytes = record.encode();
  let mut tasks = Vec::new();
  for (host, mut endpoint) in remote_holders {
    let tx = tx.clone();
    let bytes = record_bytes.clone();
    tasks.push(spawn_child(async move {
      let reply = endpoint
        .request(RECORD_STREAM, &bytes)
        .await
        .unwrap_or_default();
      let _ = tx.send(Reply::From(host, reply));
    })?);
  }
  let deadline_tx = tx.clone();
  tasks.push(spawn_child(async move {
    sleep(budget.deadline_ns).await;
    let _ = deadline_tx.send(Reply::Deadline);
  })?);
  drop(tx); // so the channel disconnects once every task has ended

  // Collect distinct, binding acknowledgements until quorum or the deadline.
  let mut timed_out = false;
  loop {
    if placement(&acked).placed(quorum) {
      break;
    }
    match rx.try_recv() {
      Ok(Reply::From(host, reply)) => {
        if let Ok(ack) = Ack::decode(&reply)
          && ack.holder == host
          && ack.binds(record)
          && candidates.contains(&host)
          && !acked.contains(&host)
        {
          acked.push(host);
        }
      }
      Ok(Reply::Deadline) => {
        timed_out = true;
        break;
      }
      Err(TryRecvError::Empty) => sleep(budget.poll_interval_ns).await,
      Err(TryRecvError::Disconnected) => break,
    }
  }

  // Cancel whatever is still running (quorum reached, or the deadline fired). A finished task's cancel
  // is a harmless miss.
  for task in tasks {
    let _ = cancel(task);
  }

  let placement = placement(&acked);
  if placement.placed(quorum) {
    Ok(placement)
  } else if timed_out {
    Err(ClusterError::Uncertain { placement })
  } else {
    Err(ClusterError::NotPlaced { placement })
  }
}
