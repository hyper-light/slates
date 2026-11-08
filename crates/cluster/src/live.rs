//! Live shipping over the fleet transport (§4.10, A-200): the owner's dispatch of its log's entries and installs to
//! its holders, and the holder's answer to each. The protocol is `slates_db::live` (the chain, the fence, the credit,
//! the durable position); this module only moves its messages and folds the replies, so the simulation that checks the
//! protocol checks what runs here.
//!
//! The dispatch is the record commit's ([`crate::commit_record_on`]): one child task per holder over its warm session,
//! each handing its reply and session back through a channel that wakes the collector ([`crate::ReplyTx`]), so a
//! write is durable the moment the `f`-th holder's acknowledgement lands. Measured over real loopback endpoints with
//! the daemon's derived runtime (`replication_bench`, 2026-11-04): see `docs/wip/BENCHMARKS.md`, "Replication
//! latency".
//!
//! A holder's follow-up (entries it lacks, an install) is not sent from inside the dispatch: the reply is folded into
//! the [`Shipper`], and the owner's period sends [`Shipper::pending`]. A write waits only for durability, never for a
//! laggard's catch-up.

use slates_db::live::{
  Admitted, Entry, Install, LiveError, Replica, Shipper, decode_reply, encode_reply,
};
use slates_db::register::HostId;
use slates_rt::futures::{detach, now_ns, spawn_child};
use slates_transport::connection::Priority;
use slates_transport::endpoint::Endpoint;

use crate::{CommitBudget, DispatchWait, Reply, Stragglers, reply_channel, request_within};

/// The request kind an entry rides on a holder's session.
/// Format: a request kind (the low bits of each exchange's stream id), distinct from every other kind of the fleet
/// plane (`RECORD_STREAM` 1 … the mirror's 21); records' class, `Control`.
pub const LIVE_ENTRY_STREAM: u64 = 22;
/// The request kind a whole-state message rides: an owner's install, or a successor's takeover fetch (A-200), told
/// apart by their tag (`slates_db::live::is_fetch`), never by guessing at the bytes.
/// Format: as [`LIVE_ENTRY_STREAM`]; a whole state is large, so it rides the `Bulk` class.
pub const LIVE_STATE_STREAM: u64 = 23;

/// What one dispatch left: the sessions that answered, for the next write, and those still in flight.
pub struct Shipped {
  /// The sessions whose holders answered before the dispatch returned.
  pub reusable: Vec<(HostId, Endpoint)>,
  /// The holders still in flight: their replies (fold them with [`fold_late`]) and sessions come back here.
  pub stragglers: Stragglers,
  /// Whether the deadline passed before `until` was durable.
  pub timed_out: bool,
}

/// Sends `message` (an encoded entry on [`LIVE_ENTRY_STREAM`], or an install on [`LIVE_STATE_STREAM`]) to each holder
/// over its session, always, and folds the replies into `shipper` until position `until` is durable (a write waits for
/// its durability alone, A-200), or with `until` `None` until every holder has answered (a catch-up, whose position is
/// usually durable already), or until the budget runs out. Returns the sessions.
///
/// Until 2026-11-04 a message whose position was already durable was not sent at all, so the owner's period never
/// caught a lagging holder up: a late holder's install was "sent" eight times and never arrived
/// (`a_holder_that_joins_late_is_installed_and_then_follows`).
pub async fn ship(
  shipper: &mut Shipper,
  (stream, message): (u64, &[u8]),
  until: Option<u64>,
  sessions: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
) -> Shipped {
  let exchanged = exchange((stream, message), sessions, budget, |host, reply| {
    let _ = shipper.on_reply(host, decode_reply(reply));
    until.is_some_and(|until| shipper.durable().is_some_and(|durable| durable >= until))
  })
  .await;
  Shipped {
    reusable: exchanged.reusable,
    stragglers: exchanged.stragglers,
    timed_out: exchanged.timed_out,
  }
}

/// What [`exchange`] gathered: every reply that came back before it returned, in arrival order, with the sessions.
pub struct Exchanged {
  /// The replies, each with the holder that sent it.
  pub replies: Vec<(HostId, Vec<u8>)>,
  /// The sessions whose holders answered.
  pub reusable: Vec<(HostId, Endpoint)>,
  /// The holders whose exchange ended with no answer at all (its deadline, or a transport fault: no round trip), their
  /// sessions not among `reusable`: a session a peer stopped answering on may be dead at the peer's end, so the caller
  /// lets its link be dialed afresh rather than lend it again.
  pub unanswered: Vec<HostId>,
  /// The holders among `unanswered` whose request a [`Watch`] ended, before its deadline.
  pub cut: Vec<HostId>,
  /// The holders still in flight.
  pub stragglers: Stragglers,
  /// Whether the budget ran out first.
  pub timed_out: bool,
}

/// What ends a live request before its deadline: its holder lost to the caller (`lost`, asked about the holder) once the
/// session has acknowledged nothing for `silence_ns`. A holder still taking the request's bytes is never cut off, so a
/// false suspicion of a live holder costs nothing; one that went quiet and was given up is not waited for to the
/// deadline, which a request's bytes set and so has no ceiling (an install measured at up to 614 s of budget).
#[derive(Clone, Copy)]
pub struct Watch {
  /// Whether the caller has stopped keeping contact with this holder.
  pub lost: fn(HostId) -> bool,
  /// How long the session must have acknowledged nothing before a lost holder's request is ended.
  pub silence_ns: u64,
}

/// [`exchange`], each request also ended as [`Watch`] says; the holders it ended that way are [`Exchanged::cut`].
pub async fn exchange_watched(
  message: (u64, &[u8]),
  sessions: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
  watch: Option<Watch>,
  enough: impl FnMut(HostId, &[u8]) -> bool,
) -> Exchanged {
  exchange_with(message, sessions, budget, watch, enough).await
}

/// One request on `endpoint`, as [`request_within`], also ended once `watch` says: `host` lost to the caller and the
/// session's largest acknowledged packet unmoved for its silence. Checked each tenth of the silence (derived: the end
/// then comes within a tenth of a silence of the moment both hold). The endpoint comes back however it ends, an
/// unfinished exchange abandoned (`StopSending`, a late copy of the reply dropped by the closed stream's tombstone), and
/// whether the watch ended it.
async fn request_watched(
  mut endpoint: Endpoint,
  host: HostId,
  (stream, priority): (u64, Priority),
  request: &[u8],
  deadline_ns: u64,
  watch: Watch,
) -> (crate::TimedReply, Endpoint, bool) {
  // Red first: the watch is not built yet; the request runs to its reply or its deadline.
  let _ = (host, watch);
  let (reply, endpoint) = request_within(endpoint, stream, priority, request, deadline_ns).await;
  (reply, endpoint, false)
}

/// Sends `message` to every holder over its session and gathers the replies until `enough` (shown each reply as it
/// lands) says so, every holder has answered, or the budget runs out. The transport half of [`ship`], for a caller whose
/// [`Shipper`] lives on another shard than its sessions (the daemon: the volume's shard appends, the control shard
/// sends): it decides from the raw replies, and the owner folds them where its shipper is.
pub async fn exchange(
  message: (u64, &[u8]),
  sessions: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
  enough: impl FnMut(HostId, &[u8]) -> bool,
) -> Exchanged {
  exchange_with(message, sessions, budget, None, enough).await
}

/// [`exchange`] and [`exchange_watched`], by whether `watch` is set.
async fn exchange_with(
  (stream, message): (u64, &[u8]),
  sessions: Vec<(HostId, Endpoint)>,
  budget: CommitBudget,
  watch: Option<Watch>,
  mut enough: impl FnMut(HostId, &[u8]) -> bool,
) -> Exchanged {
  if sessions.is_empty() {
    return Exchanged {
      replies: Vec::new(),
      reusable: sessions,
      unanswered: Vec::new(),
      cut: Vec::new(),
      stragglers: Stragglers::none(),
      timed_out: false,
    };
  }
  let priority = if stream == LIVE_STATE_STREAM {
    Priority::Bulk
  } else {
    Priority::Control
  };
  let (tx, rx) = reply_channel();
  let (cut_tx, cut_rx) = std::sync::mpsc::channel::<HostId>();
  let deadline_ns = budget.max_deadline_ns();
  let mut refused = Vec::new();
  for (host, endpoint) in sessions {
    let tx = tx.clone();
    let cut_tx = cut_tx.clone();
    let bytes = message.to_vec();
    let spawned = spawn_child(async move {
      let (reply, endpoint) = match watch {
        None => request_within(endpoint, stream, priority, &bytes, deadline_ns).await,
        Some(watch) => {
          let (reply, endpoint, cut) = request_watched(
            endpoint,
            host,
            (stream, priority),
            &bytes,
            deadline_ns,
            watch,
          )
          .await;
          if cut {
            let _ = cut_tx.send(host);
          }
          (reply, endpoint)
        }
      };
      tx.send(Reply(host, reply, Box::new(endpoint)));
    });
    // A refused spawn drops that holder's session with its future; the others still decide, and the holder is reported
    // unanswered so its link is dialed afresh rather than left lent with no session behind it.
    match spawned {
      Ok(task) => {
        let _ = detach(task);
      }
      Err(_) => refused.push(host),
    }
  }
  drop(tx);
  drop(cut_tx);
  let mut wait = DispatchWait::new(budget, now_ns());
  let mut exchanged = Exchanged {
    replies: Vec::new(),
    reusable: Vec::new(),
    unanswered: refused,
    cut: Vec::new(),
    stragglers: Stragglers::none(),
    timed_out: false,
  };
  loop {
    match rx.try_recv() {
      Ok(Reply(host, reply, endpoint)) => {
        if reply.round_trip_ns.is_none() {
          exchanged.unanswered.push(host);
          continue;
        }
        exchanged.reusable.push((host, *endpoint));
        let done = enough(host, &reply.bytes);
        exchanged.replies.push((host, reply.bytes));
        if done {
          break;
        }
      }
      Err(std::sync::mpsc::TryRecvError::Empty) => {
        if !wait.keep_waiting(exchanged.replies.len()).await {
          exchanged.timed_out = true;
          break;
        }
      }
      Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
    }
  }
  exchanged.cut = cut_rx.try_iter().collect();
  exchanged.stragglers = Stragglers::pending(rx);
  exchanged
}

/// Folds the late replies of an earlier dispatch's stragglers into `shipper`, returning their sessions and whether
/// every straggler has returned.
pub fn fold_late(
  shipper: &mut Shipper,
  stragglers: &mut Stragglers,
) -> (Vec<(HostId, Endpoint)>, bool) {
  let (replies, done) = stragglers.recover_replies();
  let sessions = replies
    .into_iter()
    .map(|(host, reply, endpoint)| {
      let _ = shipper.on_reply(host, decode_reply(&reply.bytes));
      (host, endpoint)
    })
    .collect();
  (sessions, done)
}

/// A holder's answer to an entry's bytes: checks it against `replica`, has `apply` apply its payload (the holder's
/// state; `false` refuses the entry and leaves the replica unchanged), commits, and answers with the head held.
/// Bytes over `cap` or malformed are answered with nothing, which counts as no acknowledgement.
pub fn serve_entry(
  replica: &mut Replica,
  request: &[u8],
  cap: usize,
  apply: impl FnOnce(&Entry) -> bool,
) -> Vec<u8> {
  let reply = Entry::decode(request, cap).and_then(|entry| match replica.check(&entry)? {
    Admitted::Apply(head) if apply(&entry) => {
      replica.commit(entry.epoch, head);
      Ok(Some(head))
    }
    Admitted::Apply(_) => Err(LiveError::Malformed),
    Admitted::Held => Ok(replica.head()),
  });
  encode_reply(&reply)
}

/// A holder's answer to an install's bytes, as [`serve_entry`]: `apply` replaces the holder's state with the install's.
pub fn serve_install(
  replica: &mut Replica,
  request: &[u8],
  cap: usize,
  apply: impl FnOnce(&Install) -> bool,
) -> Vec<u8> {
  let reply =
    Install::decode(request, cap).and_then(|install| match replica.check_install(&install)? {
      Admitted::Apply(head) if apply(&install) => {
        replica.commit(install.epoch, head);
        Ok(Some(head))
      }
      Admitted::Apply(_) => Err(LiveError::Malformed),
      Admitted::Held => Ok(replica.head()),
    });
  encode_reply(&reply)
}
