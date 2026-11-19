//! The version-control servers' SSH edge on the daemon's runtime (A-651; §4.17.2 "Transports" item 2 and "Pieces (git)"
//! piece 9; §4.17.3's SSH transport; §4.17.4's svn+ssh, piece 12; §4.3, R6): a listener on the runtime's TCP, one
//! connection a task, each driving `slates-ssh`'s [`Session`] (A-594, A-633, A-634) over its socket; every wait on a client
//! learned per phase ([`waits`]); the connections served at once and each one's room derived from the shard's budget
//! (A-625 part 2's [`Bounds`]); and a site ([`site`]) that proves keys to principals of §4.13 and dispatches each `exec`
//! by its tool. It is the SSH counterpart of the HTTP edge (`crate::vcs_http`), and keeps its shape.
//!
//! **The listener** ([`serve`]) is a task on its shard and owns its connections: each is a child task, detached so its
//! slot is given back as soon as it ends, and cancelled and joined when the listener ends, whoever ends it, which is the
//! runtime's rule for a task's children. So no connection served on the listener's shard outlives its listener (banned
//! item 9). A connection that moved to another shard is a task of that shard's, recorded in the seat it took there, so
//! that shard's reap or its runtime's end cancels it.
//!
//! **A connection moves to its repository's shard** (A-651 part 4; main's ruling for the HTTP edge, A-625 part 3).
//! Once an `exec` names a repository another shard's partition holds ([`site::SshSite::locate`]: the name routes to its
//! partition, D-14), the connection goes there whole, as the HTTP edge moves one once a head names its repository: its
//! socket's handle, its session (the transport's keys and sequence numbers, the channel, and whatever the client sent
//! after the `exec`, not yet read), the client's bytes the session has not taken, its waits, its phase and its proven
//! principal, with the `exec` undecided, as one spawn on that shard's control channel, sharing by move. Its seat here is
//! given back first, and there it takes one of that shard's seats as a new connection would: a free one, or the most
//! overdue connection's, or it waits, and at most as many arrivals wait as the shard has seats, past which the arrival
//! is closed and counted, as the HTTP edge closes one. There the `exec` is dispatched and answered, so the repository's
//! record, its policy and its store are read on their own shard with no crossing. Each shard keeps its own edge state
//! ([`keep_here`]): its seats, learned waits, lifetimes and counts; only the listener's holds the host key, since only
//! it makes sessions. A command that names no repository (Subversion's tunnel, whose client names one in its greeting)
//! is dispatched where its connection was accepted.
//!
//! **Bounds, derived** (A-625 part 2's [`Bounds::of_shard`]). An SSH connection is a client of its shard: the edge serves
//! as many at once as the shard seats clients, and each may hold what one client's region holds, which is its channel's
//! window (`SessionConfig::channel_window`): the client's bytes the server holds unconsumed. Beside it a connection holds
//! one read ([`READ_BYTES`]) and the session's own buffers, each about a packet cap. The packet cap is the one every peer
//! accepts ([`PACKET_CAP`]). The kernel's queue of connections not yet accepted is the same bound as the seats
//! (`Bounds::backlog`).
//!
//! **Overdue, and reaped for a new connection** (main's rulings, 2026-11-19, the HTTP edge's rule). A wait past its
//! learned bound ([`waits`]) marks its connection overdue in its seat and goes on. The listener accepts the next
//! connection whenever it comes; when every seat is taken it reaps the most overdue connection for it (its task
//! cancelled, its seat given back as its guard drops; the reap teaches nothing and backs the phase off), and with none
//! overdue it waits until a seat comes back or a connection becomes overdue. So the listener holds at most one accepted
//! connection beyond the seats, and none is cut while no new connection needs its seat, because an SSH connection waits
//! on people inside it: OpenSSH asks for a key's passphrase only after the server's `SSH_MSG_USERAUTH_PK_OK`
//! (`input_userauth_pk_ok` calls `sign_and_send_pubkey`, whose `identity_sign` loads a key not yet loaded through
//! `load_identity_file`, which prompts "Enter passphrase for key") \[C: OpenSSH `sshconnect2.c` at `fa4b2a82`, lines 745,
//! 1242 and 1543\], asks a person to confirm an unknown host key before its `SSH_MSG_NEWKEYS` ("Are you sure you want to
//! continue connecting", `check_host_key` in `sshconnect.c`) \[C\], and a
//! push's client compresses its pack before it sends a byte of it. A cut at a bound learned from the clients that
//! finished would cut every agent-less passphrase on every connection, and a cut before the proof backs nothing off, so
//! it would never be learned (g294's finding; main's ruling).
//!
//! **One connection's loop.** It writes what the session has to send; takes the session's events; asks the command for
//! its output while the channel has room; and otherwise reads the client's bytes, within the session's own timer
//! (rekeying by time). The phases move as the session does: the handshake ends when the first exchange gives the session
//! its id, authentication with the principal's proof, setup with the `exec`, which the site dispatches, the command with
//! its exit (`exit-status`, EOF and close, in order), and closing with the channel's close, after which the connection
//! ends. A refused `exec` is answered as its tool's program refuses: the message on standard error, then its exit
//! status. One command output run, and one client input run, a turn (§4.3): the task yields after each.
//!
//! **The key time** (A-594's row of §4.17.2's tunables): each session's key is replaced by time at the tail of the
//! lifetimes this edge's authenticated connections lived (RFC 6298's estimator), the hour before any
//! (`slates_ssh::keylife::key_time_limit_ns`). Only a connection that ended as its client ended it is a sample.
//!
//! **Owed** (GAPS, the §4.17 entry): the daemon starting an edge (its address, the host key's keeping, an edge state
//! kept on every shard, the site's services over A-682's store lender and the shard's state); the services still
//! refused as not served, each until it lands (Mercurial's `serve --stdio`, A-679; Subversion's tunnel, §4.17.4 piece
//! 12), git's upload-pack (`git`, its store lent by A-682's `ShardStores`) and receive-pack (`receive`, A-654 part 2's
//! driver, A-683) running already; Subversion's tunnel moved to the shard of the repository its greeting names; the
//! live `git clone` and `git push` over SSH (main's runs).

mod git;
mod receive;
pub(crate) mod site;
mod waits;

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::task::{Poll, Waker};

use slates_cluster::failover::RunningDuration;
use slates_db::catalog::Principal;
use slates_rt::TaskId;
use slates_rt::error::RtError;
use slates_rt::futures::{cancel, detach, now_ns, sleep, spawn_child, yield_now};
use slates_rt::shard::Kept;
use slates_rt::tcp::{OwnedStream, TcpListener, TcpStream};
use slates_ssh::command::Command as Asked;
use slates_ssh::entropy::SystemEntropy;
use slates_ssh::hostkey::{ED25519_SEED_LEN, HostKey};
use slates_ssh::keylife::{UsageCap, key_time_limit_ns};
use slates_ssh::packet::MAX_PACKET_EVERY_PEER_ACCEPTS;
use slates_ssh::session::{Session, SessionConfig, SessionEvent};
use slates_ssh::transport::TransportConfig;
use slates_ssh::userauth::{KeyHolders, RECOMMENDED_FAILURE_LIMIT};

use crate::vcs_http::Bounds;
use crate::vcs_http::git::Located;
use site::{Command, Dispatched, Next, Refused, SshSite};
use waits::{Overdue, Phase, PhaseWaits, Tracker};

/// Derived: the transport's packet cap: the size every peer accepts (RFC 4253 §6.1: "All implementations MUST be able to
/// process packets with ... a total packet size of 35000 bytes or less"). A larger cap buys nothing from OpenSSH, whose
/// session channels carry packets of 32 KiB (`CHAN_SES_PACKET_DEFAULT` in `channels.h`) \[C\], and every byte past it is
/// held per connection.
const PACKET_CAP: usize = MAX_PACKET_EVERY_PEER_ACCEPTS;
/// Derived: the bytes one read takes from a connection's socket: a packet cap, so a read never splits a packet it could
/// have taken whole.
const READ_BYTES: usize = PACKET_CAP;

/// Format: the refusal counts the edge keeps in the daemon's status report beside its own [`Counts`] (banned item 9:
/// every connection ended other than by its client is counted by its reason).
const SPAWN_REFUSED: &str = "vcs_ssh.spawn";
/// A connection task that could not be detached, which keeps its slot until its listener ends.
const NOT_DETACHED: &str = "vcs_ssh.detach";
/// A connection no session could be made for.
const NO_SESSION: &str = "vcs_ssh.no_session";
/// A connection whose round trip the kernel could not give.
const NO_ROUND_TRIP: &str = "vcs_ssh.no_round_trip";
/// An overdue connection reaped for a new one.
const REAPED: &str = "vcs_ssh.reaped";
/// A reaped connection whose task could not be cancelled (already gone).
const NOT_REAPED: &str = "vcs_ssh.not_reaped";
/// A connection whose socket failed.
const SOCKET_FAILED: &str = "vcs_ssh.socket";
/// A move its target shard's control channel refused: the socket's handle dropped with the spawn, so the connection
/// closed.
const MOVE_REFUSED: &str = "vcs_ssh.move_refused";
/// A connection that arrived on a shard keeping no edge state, or whose socket could not be adopted there.
const ARRIVAL_LOST: &str = "vcs_ssh.arrival_lost";
/// A connection that arrived on a shard whose seats were all taken with as many arrivals already waiting as it has
/// seats: closed.
const ARRIVAL_REFUSED: &str = "vcs_ssh.arrival_refused";

/// Why an edge cannot serve, or stopped serving.
#[derive(Debug)]
pub(crate) enum EdgeRefused {
  /// The edge's state is not on the shard serving it: kept on another shard, or its shard gone.
  StateGone,
  /// The listener failed.
  Listener(RtError),
}

impl std::fmt::Display for EdgeRefused {
  /// The refusal as the daemon's log line names it.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      EdgeRefused::StateGone => formatter.write_str("the SSH edge's state is not on its shard"),
      EdgeRefused::Listener(error) => write!(formatter, "the SSH edge's listener failed: {error}"),
    }
  }
}

/// The host key's seed, overwritten with zeros when dropped (as `slates_ssh::kex::Secret` is).
struct HostSeed([u8; ED25519_SEED_LEN]);

impl Drop for HostSeed {
  fn drop(&mut self) {
    self.0.fill(0);
    std::hint::black_box(&self.0);
  }
}

/// What an edge has counted: its non-vacuity counters, which its tests read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
  /// Connections accepted.
  pub(crate) accepted: u64,
  /// Times the listener held an accepted connection with every seat taken.
  pub(crate) full: u64,
  /// Waits that passed their bound, marking their connection overdue.
  pub(crate) overdue: u64,
  /// Overdue connections reaped for a new one.
  pub(crate) reaped: u64,
  /// Connections whose socket failed, or for which no session or round trip could be had.
  pub(crate) failed: u64,
  /// Connections the session refused (a protocol error, a failed exchange, too many authentication failures).
  pub(crate) refused: u64,
  /// `exec`s refused by the site.
  pub(crate) commands_refused: u64,
  /// Commands run to their exit.
  pub(crate) commands_run: u64,
  /// Connections this shard moved to their repository's shard.
  pub(crate) moved: u64,
  /// Connections that moved to this shard and took a seat here.
  pub(crate) arrived: u64,
  /// Connections that moved to this shard and were closed with every seat taken and the arrivals' bound reached.
  pub(crate) arrivals_refused: u64,
}

/// A taken seat: its connection's task once spawned, whether its wait on its client is past its bound, and whether it is
/// being reaped.
#[derive(Clone, Copy, Debug)]
struct Taken {
  /// The connection's task, once spawned.
  task: Option<TaskId>,
  /// Its wait's mark while it waits past its bound.
  overdue: Option<Overdue>,
  /// Whether it has been chosen to be reaped: its seat comes back as its task's cancellation drops it.
  reaping: bool,
}

/// How the listener stands for a seat ([`EdgeState::seating`]).
enum Seating {
  /// This seat is free.
  Free(usize),
  /// None is free and this connection's task, the most overdue, is to be reaped for the one waiting (none when it was
  /// never spawned, whose seat comes back as its spawn is refused).
  Reap(Option<TaskId>),
  /// None is free and none is overdue: wait for a seat to come back or a connection to become overdue.
  Wait,
}

/// An edge's state on its shard, shared by its listener and its connections through a [`Kept`] handle: what bounds it,
/// its host key, its seats, who waits for one, every phase's learned waits, the connections' lifetimes, and its counts.
/// Nothing in it is lent across a wait.
pub(crate) struct EdgeState {
  /// The edge's bounds.
  bounds: Bounds,
  /// The host key's seed: each connection's key is made from it. None on a shard that only seats connections that moved
  /// there ([`EdgeState::arrivals`]), which makes no session.
  host_seed: Option<HostSeed>,
  /// Each seat, free or taken: as many as the edge serves at once.
  seats: RefCell<Vec<Option<Taken>>>,
  /// Who waits for a seat: the listener, and connections that moved here, at most as many of them as the shard has
  /// seats ([`EdgeState::arriving`]), each kept once.
  waiting: RefCell<Vec<Waker>>,
  /// The connections that moved here and wait for a seat: at most as many as the shard has seats.
  arriving: Cell<usize>,
  /// Every phase's learned waits.
  waits: RefCell<PhaseWaits>,
  /// How long the authenticated connections lived.
  lifetimes: RefCell<RunningDuration>,
  /// The counts.
  counts: Cell<Counts>,
}

impl EdgeState {
  /// An edge's state on its listener's shard: `bounds`, and the seed of its Ed25519 host key.
  pub(crate) fn new(bounds: Bounds, host_seed: [u8; ED25519_SEED_LEN]) -> EdgeState {
    EdgeState::of(bounds, Some(HostSeed(host_seed)))
  }

  /// An edge's state on a shard that only seats connections that moved there (the module doc): `bounds`, and no host
  /// key, since it makes no session.
  pub(crate) fn arrivals(bounds: Bounds) -> EdgeState {
    EdgeState::of(bounds, None)
  }

  /// An edge's state of `bounds` whose sessions' host key is made from `host_seed`, if it makes any.
  fn of(bounds: Bounds, host_seed: Option<HostSeed>) -> EdgeState {
    EdgeState {
      bounds,
      host_seed,
      seats: RefCell::new(vec![None; bounds.connections]),
      waiting: RefCell::new(Vec::new()),
      arriving: Cell::new(0),
      waits: RefCell::new(PhaseWaits::default()),
      lifetimes: RefCell::new(RunningDuration::new()),
      counts: Cell::new(Counts::default()),
    }
  }

  /// What the edge has counted.
  pub(crate) fn counts(&self) -> Counts {
    self.counts.get()
  }

  /// The connections being served: the seats taken.
  pub(crate) fn serving(&self) -> usize {
    self
      .seats
      .try_borrow()
      .map_or(0, |seats| seats.iter().flatten().count())
  }

  /// How many of `phase`'s waits completed and how many of its connections were reaped; nothing while the waits are lent,
  /// which is never across a wait.
  pub(crate) fn learned(&self, phase: Phase) -> (u64, u64) {
    self
      .waits
      .try_borrow()
      .map_or((0, 0), |waits| (waits.samples(phase), waits.reaps(phase)))
  }

  /// How many back-offs `phase` has taken from reaps of proven connections; nothing while the waits are lent.
  pub(crate) fn backoffs(&self, phase: Phase) -> u64 {
    self
      .waits
      .try_borrow()
      .map_or(0, |waits| waits.generation(phase))
  }

  /// Derived: the key time a new session's key is replaced at (the module doc).
  pub(crate) fn key_time_ns(&self) -> u64 {
    let tail = self
      .lifetimes
      .try_borrow()
      .ok()
      .and_then(|lifetimes| lifetimes.tail_ns());
    key_time_limit_ns(tail)
  }

  /// Changes the counts.
  fn count(&self, change: impl FnOnce(&mut Counts)) {
    let mut counts = self.counts.get();
    change(&mut counts);
    self.counts.set(counts);
  }

  /// Runs `f` over the learned waits; none while they are lent, which is never across a wait.
  fn with_waits<R>(&self, f: impl FnOnce(&mut PhaseWaits) -> R) -> Option<R> {
    self
      .waits
      .try_borrow_mut()
      .ok()
      .map(|mut waits| f(&mut waits))
  }

  /// Runs `f` over the seats; none while they are lent, which is never across a wait.
  fn with_seats<R>(&self, f: impl FnOnce(&mut Vec<Option<Taken>>) -> R) -> Option<R> {
    self
      .seats
      .try_borrow_mut()
      .ok()
      .map(|mut seats| f(&mut seats))
  }

  /// Wakes whoever waits for a seat: each looks again ([`seat_for`]).
  fn wake_listener(&self) {
    let waiting = self
      .waiting
      .try_borrow_mut()
      .map(|mut waiting| std::mem::take(&mut *waiting))
      .unwrap_or_default();
    for waker in waiting {
      waker.wake();
    }
  }

  /// Keeps `waker` to be woken when a seat comes back or a connection becomes overdue, once.
  fn wait_for_seat(&self, waker: &Waker) {
    if let Ok(mut waiting) = self.waiting.try_borrow_mut()
      && !waiting.iter().any(|kept| kept.will_wake(waker))
    {
      waiting.push(waker.clone());
    }
  }

  /// Admits one more connection that moved here to wait for a seat, while fewer than the shard's seats wait.
  fn admit_arrival(&self) -> bool {
    let arriving = self.arriving.get();
    if arriving >= self.bounds.connections {
      return false;
    }
    self.arriving.set(arriving.saturating_add(1));
    true
  }

  /// One connection that waited here for a seat stopped waiting.
  fn arrival_seated(&self) {
    self.arriving.set(self.arriving.get().saturating_sub(1));
  }

  /// How a connection waiting for a seat, the listener's accepted one or one that moved here, stands (the module doc): a
  /// free seat; or, with every seat taken, the most overdue connection not already being reaped, chosen now (its reap
  /// counted, teaching nothing and backing its phase off); or a wait, its `waker` kept to be woken when a seat comes back
  /// or a connection becomes overdue. A first look with every seat taken (`first`) is counted.
  fn seating(&self, waker: &Waker, first: bool) -> Seating {
    let Some((seating, reaped)) = self.with_seats(|seats| choose_seat(seats)) else {
      return Seating::Wait;
    };
    if matches!(seating, Seating::Free(_)) {
      return seating;
    }
    self.wait_for_seat(waker);
    if first {
      self.count(|counts| counts.full = counts.full.saturating_add(1));
    }
    if let Some(overdue) = reaped {
      let _ = self.with_waits(|waits| waits.reap(&overdue));
      self.count(|counts| counts.reaped = counts.reaped.saturating_add(1));
    }
    seating
  }

  /// Takes seat `slot` for a connection just accepted.
  fn take_seat(&self, slot: usize) {
    let _ = self.with_seats(|seats| {
      if let Some(seat) = seats.get_mut(slot) {
        *seat = Some(Taken {
          task: None,
          overdue: None,
          reaping: false,
        });
      }
    });
  }

  /// Records the task serving seat `slot`.
  fn set_task(&self, slot: usize, task: TaskId) {
    let _ = self.with_seats(|seats| {
      if let Some(Some(taken)) = seats.get_mut(slot) {
        taken.task = Some(task);
      }
    });
  }

  /// Marks seat `slot`'s connection overdue as `overdue` says, or clears its mark (`None`); a mark wakes whoever waits
  /// for a seat: the listener holding a connection, or a connection that moved here.
  fn mark(&self, slot: usize, overdue: Option<Overdue>) {
    let marked = self
      .with_seats(|seats| match seats.get_mut(slot) {
        Some(Some(taken)) if !taken.reaping => {
          taken.overdue = overdue;
          true
        }
        _ => false,
      })
      .unwrap_or(false);
    if marked && overdue.is_some() {
      self.count(|counts| counts.overdue = counts.overdue.saturating_add(1));
      self.wake_listener();
    }
  }

  /// Gives seat `slot` back, waking whoever waits for one.
  fn give_back(&self, slot: usize) {
    let _ = self.with_seats(|seats| {
      if let Some(seat) = seats.get_mut(slot) {
        *seat = None;
      }
    });
    self.wake_listener();
  }

  /// A session for a connection accepted at `now_ns` whose keys' lookup is `keys`; none when none can be made, or this
  /// shard's edge holds no host key ([`EdgeState::arrivals`]).
  fn session<K: KeyHolders>(&self, keys: K, now_ns: u64) -> Option<Session<SystemEntropy, K>> {
    let host_key = HostKey::ed25519_from_seed(&self.host_seed.as_ref()?.0).ok()?;
    let config = SessionConfig {
      transport: TransportConfig {
        packet_cap: PACKET_CAP,
        banner: Vec::new(),
        comments: None,
        key_time_ns: self.key_time_ns(),
        usage_cap: UsageCap::default(),
        extensions: Vec::new(),
      },
      banner: None,
      failure_limit: RECOMMENDED_FAILURE_LIMIT,
      channel_window: u32::try_from(self.bounds.room).unwrap_or(u32::MAX),
    };
    Session::new(config, host_key, SystemEntropy::new(), keys, now_ns).ok()
  }
}

/// A free seat in `seats`; or, with every seat taken, the most overdue connection not already being reaped, marked as
/// being reaped, with its mark: the earliest deadline passed is the most overdue; or none to reap.
fn choose_seat(seats: &mut [Option<Taken>]) -> (Seating, Option<Overdue>) {
  if let Some(free) = seats.iter().position(Option::is_none) {
    return (Seating::Free(free), None);
  }
  let most_overdue = seats
    .iter_mut()
    .flatten()
    .filter(|taken| !taken.reaping)
    .filter_map(|taken| taken.overdue.map(|overdue| (overdue, taken)))
    .min_by_key(|(overdue, _)| overdue.since_ns);
  match most_overdue {
    Some((overdue, taken)) => {
      taken.reaping = true;
      taken.overdue = None;
      (Seating::Reap(taken.task), Some(overdue))
    }
    None => (Seating::Wait, None),
  }
}

/// A connection's seat among those its edge serves at once: taken as it is given a seat and given back when this drops,
/// however the connection leaves (its loop's end, its move to another shard, its task's cancellation by a reap or with
/// its listener, a refused spawn), whoever waits for one woken.
struct Seat {
  /// The edge.
  state: Kept<EdgeState>,
  /// Which seat.
  slot: usize,
}

impl Seat {
  /// Takes seat `slot` on the edge `state` keeps; none off its shard.
  fn take(state: Kept<EdgeState>, slot: usize) -> Option<Seat> {
    state
      .with(|state| state.take_seat(slot))
      .map(|()| Seat { state, slot })
  }
}

impl Drop for Seat {
  fn drop(&mut self) {
    let slot = self.slot;
    // Off its shard (the runtime gone) no edge is left to give the seat back to.
    let _ = self.state.with(|state| state.give_back(slot));
  }
}

thread_local! {
  /// This shard's edge state, once kept here ([`keep_here`]): where a connection that moves here is seated.
  static HERE: Cell<Option<Kept<EdgeState>>> = const { Cell::new(None) };
}

/// Keeps `state` on this shard for its life as the shard's SSH edge (the module doc): the listener's shard keeps the
/// one with the host key ([`EdgeState::new`]), every other shard one without ([`EdgeState::arrivals`]), so a connection
/// that moves to a shard finds its seats there. Called once a shard, at the daemon's start; refused off a shard's
/// thread.
pub(crate) fn keep_here(state: EdgeState) -> Result<Kept<EdgeState>, RtError> {
  let kept = slates_rt::registry::with_current(|context| context.keep(state))
    .ok_or(RtError::NotOnShardThread)??;
  HERE.with(|here| here.set(Some(kept)));
  Ok(kept)
}

/// This shard's edge state, if one was kept here.
pub(crate) fn here() -> Option<Kept<EdgeState>> {
  HERE.with(Cell::get)
}

/// Waits, holding an accepted connection, until a seat is free for it (the module doc): at once below the bound; with
/// every seat taken, by reaping the most overdue connection, or once a seat comes back or a connection becomes overdue.
async fn seat_for(state: Kept<EdgeState>) -> Result<usize, EdgeRefused> {
  let mut first = true;
  std::future::poll_fn(|cx| {
    let seating = state.with(|state| state.seating(cx.waker(), first));
    first = false;
    match seating {
      Some(Seating::Free(slot)) => Poll::Ready(Ok(slot)),
      Some(Seating::Reap(task)) => {
        // Its seat comes back, waking this listener, as the cancellation drops its task.
        if let Some(task) = task
          && cancel(task).is_err()
        {
          crate::fleet::count_refusal(NOT_REAPED);
        }
        crate::fleet::count_refusal(REAPED);
        Poll::Pending
      }
      Some(Seating::Wait) => Poll::Pending,
      None => Poll::Ready(Err(EdgeRefused::StateGone)),
    }
  })
  .await
}

/// Serves the edge `state` keeps on `listener` until the listener fails or its task ends (the module doc): each
/// connection a child task, its keys and commands those of a site `site` makes for it, here or on the shard it moves to.
/// Refused at once when `state` is not on this shard.
pub(crate) async fn serve<S, M>(
  listener: TcpListener,
  state: Kept<EdgeState>,
  site: M,
) -> Result<(), EdgeRefused>
where
  S: SshSite + 'static,
  S::Keys: Send + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  state.with(|_| ()).ok_or(EdgeRefused::StateGone)?;
  loop {
    let stream = listener.accept().await.map_err(EdgeRefused::Listener)?;
    let _ =
      state.with(|state| state.count(|counts| counts.accepted = counts.accepted.saturating_add(1)));
    let Ok(round_trip) = stream.round_trip() else {
      let _ =
        state.with(|state| state.count(|counts| counts.failed = counts.failed.saturating_add(1)));
      crate::fleet::count_refusal(NO_ROUND_TRIP);
      continue;
    };
    let slot = seat_for(state).await?;
    let seat = Seat::take(state, slot).ok_or(EdgeRefused::StateGone)?;
    let tracker = Tracker::new(round_trip);
    match spawn_child(drive(stream, site.clone(), seat, tracker)) {
      Ok(task) => {
        let _ = state.with(|state| state.set_task(slot, task));
        if detach(task).is_err() {
          crate::fleet::count_refusal(NOT_DETACHED);
        }
      }
      // The refused task's future, its seat with it, was dropped: the seat is given back.
      Err(_) => {
        crate::fleet::count_refusal(SPAWN_REFUSED);
      }
    }
  }
}

/// How a wait on a client ended: never at its bound, which only marks its connection overdue.
enum Waited<T> {
  /// It completed with this.
  Done(T),
  /// The session's own timer came first (a rekey by time): the wait goes on after it.
  Timer,
  /// No deadline could be armed (off the shard).
  Failed,
}

/// Awaits `work` with no end of its own: once `deadline_ns` passes with it still waiting, `overdue` is called, once, and
/// the same work goes on (never dropped mid-way, so a write past its deadline loses no byte it handed the kernel); and
/// once the session's timer `timer_ns` passes, the work is set aside for the session to run it. What ended the wait, and
/// whether it was overdue.
async fn awaited<F: Future>(
  work: F,
  deadline_ns: u64,
  timer_ns: Option<u64>,
  mut overdue: impl FnMut(),
) -> (Waited<F::Output>, bool) {
  let now = now_ns();
  let mut work = std::pin::pin!(work);
  let mut deadline = std::pin::pin!(sleep(deadline_ns.saturating_sub(now)));
  let mut timer = std::pin::pin!(sleep(
    timer_ns.map_or(0, |timer_ns| timer_ns.saturating_sub(now))
  ));
  let mut passed = false;
  std::future::poll_fn(|cx| {
    if let Poll::Ready(output) = work.as_mut().poll(cx) {
      return Poll::Ready((Waited::Done(output), passed));
    }
    if !passed {
      match deadline.as_mut().poll(cx) {
        Poll::Ready(Ok(())) => {
          passed = true;
          overdue();
        }
        Poll::Ready(Err(_)) => return Poll::Ready((Waited::Failed, passed)),
        Poll::Pending => {}
      }
    }
    if timer_ns.is_some() {
      match timer.as_mut().poll(cx) {
        Poll::Ready(Ok(())) => return Poll::Ready((Waited::Timer, passed)),
        Poll::Ready(Err(_)) => return Poll::Ready((Waited::Failed, passed)),
        Poll::Pending => {}
      }
    }
    Poll::Pending
  })
  .await
}

/// Why a connection's loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
  /// Its client closed it, disconnected, or closed the channel: as the client meant.
  Closed,
  /// The session refused the client, and said why.
  Refused,
  /// The socket failed, or the edge's state left its shard.
  Failed,
}

/// How a connection's loop left: it ended, or it moves to the shard its repository lives on (the module doc).
enum Exit {
  /// It ended.
  Ended(Why),
  /// It moves to runtime shard `shard`, its `exec` not yet dispatched.
  Moved {
    /// The shard.
    shard: u16,
    /// The `exec`, dispatched there.
    exec: Exec,
  },
}

/// An `exec` a connection carries to the shard its repository lives on, where it is dispatched and answered.
struct Exec {
  /// The command.
  command: Asked,
  /// The `GIT_PROTOCOL` value sent before it.
  git_protocol: Option<Vec<u8>>,
}

/// A running command, or the refusal standing in for one, and what it waits for.
struct Running<C> {
  /// The command; none for a refused `exec` or a failed command, whose words alone are sent.
  command: Option<C>,
  /// A refusal's or a failure's words not yet sent, on standard error.
  refusal: Vec<u8>,
  /// What it waits for.
  next: Next,
  /// Whether the channel took nothing it was offered: held until the session says it is writable.
  held: bool,
}

impl<C: Command> Running<C> {
  /// A command just started.
  fn started(command: C) -> Self {
    Self {
      command: Some(command),
      refusal: Vec::new(),
      next: Next::More,
      held: false,
    }
  }

  /// A refused `exec`: its words, then its status.
  fn refused(refused: Refused) -> Self {
    Self {
      command: None,
      refusal: refused.stderr,
      next: Next::Exited(refused.exit_status),
      held: false,
    }
  }

  /// Whether anything it wrote is not yet sent.
  fn unsent(&self) -> bool {
    match &self.command {
      Some(command) => {
        let (stderr, stdout) = command.output();
        !stderr.is_empty() || !stdout.is_empty()
      }
      None => !self.refusal.is_empty(),
    }
  }

  /// Whether it is still taking input: a command that has not exited.
  fn taking(&mut self) -> Option<&mut C> {
    if matches!(self.next, Next::Exited(_)) {
      return None;
    }
    self.command.as_mut()
  }
}

/// What one turn of the command did.
enum Turn {
  /// It moved: output written to the channel, or the command asked again.
  Moved,
  /// It waits on the client: for input, or for the channel's room.
  Client,
  /// Nothing runs.
  Idle,
}

/// One connection: its socket, its session, its site, its waits, the principal its key proved, its command, and when
/// it was accepted.
struct Link<S: SshSite> {
  /// The socket.
  stream: TcpStream,
  /// One read's buffer.
  buffer: Vec<u8>,
  /// The client's bytes read and not yet taken by the session.
  unread: Vec<u8>,
  /// The session.
  session: Session<SystemEntropy, S::Keys>,
  /// The site.
  site: S,
  /// The edge.
  state: Kept<EdgeState>,
  /// The connection's seat.
  slot: usize,
  /// The connection's waits.
  tracker: Tracker,
  /// The phase it is in.
  phase: Phase,
  /// The principal its key proved.
  principal: Option<Principal>,
  /// Its command, once its `exec` was dispatched.
  running: Option<Running<S::Command>>,
  /// When its listener accepted it, on whichever shard it is served now: its lifetime's start.
  accepted_ns: u64,
}

impl<S: SshSite> Link<S> {
  /// Runs `f` over the connection's waits and the edge's learned ones; none off the edge's shard.
  fn learning<R>(&mut self, f: impl FnOnce(&mut Tracker, &mut PhaseWaits) -> R) -> Option<R> {
    let tracker = &mut self.tracker;
    self
      .state
      .with(|state| state.with_waits(|waits| f(&mut *tracker, waits)))
      .flatten()
  }

  /// The connection moves into `phase` now.
  fn enter(&mut self, phase: Phase) {
    let now = now_ns();
    self.phase = phase;
    let _ = self.learning(|tracker, waits| tracker.observe(phase, now, waits));
  }

  /// Where a wait in the connection's phase starting now passes its bound ([`Tracker::deadline_ns`]), and the mark its
  /// connection takes then: `Failed` off the edge's shard, where no wait can be kept.
  fn armed(&mut self) -> Result<(u64, Overdue), Why> {
    let (now, phase) = (now_ns(), self.phase);
    let granularity_ns = self
      .state
      .with(|state| state.bounds.granularity_ns())
      .ok_or(Why::Failed)?;
    let deadline_ns = self
      .learning(|tracker, waits| tracker.deadline_ns(phase, now, granularity_ns, waits))
      .ok_or(Why::Failed)?;
    Ok((deadline_ns, self.tracker.overdue(phase, deadline_ns)))
  }

  /// What marks the connection's seat overdue as `overdue` says, when its wait passes its bound.
  fn marker(&self, overdue: Overdue) -> impl FnMut() + use<S> {
    let (state, slot) = (self.state, self.slot);
    move || {
      let _ = state.with(|state| state.mark(slot, Some(overdue)));
    }
  }

  /// A wait ended: its overdue mark, if it took one, cleared; and, completed, a running silence heard (however late).
  fn settle<T>(&mut self, (waited, overdue): (Waited<T>, bool)) -> Result<Option<T>, Why> {
    if overdue {
      let slot = self.slot;
      let _ = self.state.with(|state| state.mark(slot, None));
    }
    match waited {
      Waited::Done(value) => {
        let now = now_ns();
        let _ = self.learning(|tracker, waits| tracker.heard(now, waits));
        Ok(Some(value))
      }
      Waited::Timer => Ok(None),
      Waited::Failed => {
        self.tracker.abandon();
        Err(Why::Failed)
      }
    }
  }

  /// Runs the connection until it ends or moves to its repository's shard (the module doc's loop).
  async fn run(&mut self) -> Exit {
    loop {
      if !self.session.pending_output().is_empty() {
        if let Err(why) = self.write().await {
          return Exit::Ended(why);
        }
        continue;
      }
      if self.session.is_closed() {
        return Exit::Ended(Why::Refused);
      }
      if let Some(event) = self.session.poll_event() {
        match self.on_event(event).await {
          Some(exit) => return exit,
          None => continue,
        }
      }
      if !self.unread.is_empty() && self.feed() {
        continue;
      }
      match self.turn().await {
        Ok(Turn::Moved) => continue,
        Ok(Turn::Client | Turn::Idle) => {}
        Err(why) => return Exit::Ended(why),
      }
      if let Err(why) = self.read().await {
        return Exit::Ended(why);
      }
    }
  }

  /// Writes what the session has to send, within the phase's wait: the client's room is waited for.
  async fn write(&mut self) -> Result<(), Why> {
    let (deadline_ns, overdue) = self.armed()?;
    let bytes = self.session.pending_output().to_vec();
    let marker = self.marker(overdue);
    let timer_ns = self.session.next_timer_ns();
    let outcome = awaited(self.stream.write_all(&bytes), deadline_ns, timer_ns, marker).await;
    match self.settle(outcome)? {
      Some(Ok(())) => self
        .session
        .consume_output(bytes.len())
        .map_err(|_| Why::Refused),
      Some(Err(_)) => {
        self.tracker.abandon();
        Err(Why::Failed)
      }
      // The write was set aside whole for the session's timer: none of it was taken, and it is written again.
      None => self.session.poll_timers(now_ns()).map_err(|_| Why::Refused),
    }
  }

  /// Reads the client's bytes into the session, within the phase's wait and the session's own timer; the client's close
  /// ends the loop.
  async fn read(&mut self) -> Result<(), Why> {
    let (deadline_ns, overdue) = self.armed()?;
    let marker = self.marker(overdue);
    let timer_ns = self.session.next_timer_ns();
    let outcome = awaited(
      self.stream.read(&mut self.buffer),
      deadline_ns,
      timer_ns,
      marker,
    )
    .await;
    match self.settle(outcome)? {
      Some(Ok(0)) => {
        // The client left: a closing connection's wait ends as it should; any other wait's true length is unknown.
        if self.phase == Phase::Closing {
          let now = now_ns();
          let _ = self.learning(|tracker, waits| tracker.end(now, waits));
        } else {
          self.tracker.abandon();
        }
        Err(Why::Closed)
      }
      Some(Ok(count)) => {
        self
          .unread
          .extend_from_slice(self.buffer.get(..count).unwrap_or_default());
        self.feed();
        Ok(())
      }
      Some(Err(_)) => {
        self.tracker.abandon();
        Err(Why::Failed)
      }
      None => self.session.poll_timers(now_ns()).map_err(|_| Why::Refused),
    }
  }

  /// Hands the unread bytes to the session: whether it took any. The edge's own work on the handshake is counted into
  /// the connection's first measurement, and the handshake ends when the bytes established the transport.
  fn feed(&mut self) -> bool {
    let started_ns = now_ns();
    let taken = match self.session.receive(&self.unread, started_ns) {
      Ok(taken) => taken,
      Err(_) => {
        // The session refused the client and queued its disconnect: nothing more of the client's is read.
        self.unread.clear();
        return true;
      }
    };
    let worked_ns = now_ns().saturating_sub(started_ns);
    self.tracker.worked(worked_ns);
    self.unread.drain(..taken.min(self.unread.len()));
    if self.phase == Phase::Handshake && self.session.transport().is_established() {
      self.enter(Phase::Authentication);
    }
    taken > 0
  }

  /// Takes one session event: the loop's exit when it ends the connection or moves it.
  async fn on_event(&mut self, event: SessionEvent<Principal>) -> Option<Exit> {
    match event {
      SessionEvent::Authenticated(authenticated) => {
        self.principal = Some(authenticated.principal);
        let _ = self.learning(|tracker, waits| tracker.prove(waits));
        self.enter(Phase::Setup);
      }
      SessionEvent::KeyWanted(wanted) => {
        let holding = self
          .site
          .lookup(&wanted.user, &wanted.key, &wanted.blob)
          .await;
        if self.session.key_resolved(holding).is_err() {
          return Some(Exit::Ended(Why::Refused));
        }
      }
      SessionEvent::Exec {
        command,
        git_protocol,
      } => return self.located(command, git_protocol),
      SessionEvent::Data(run) => return self.on_data(&run).await.map(Exit::Ended),
      SessionEvent::Eof => self.on_eof(),
      SessionEvent::Writable => {
        if let Some(running) = self.running.as_mut() {
          running.held = false;
        }
      }
      SessionEvent::ChannelClosed => {
        let now = now_ns();
        let _ = self.learning(|tracker, waits| tracker.end(now, waits));
        return Some(Exit::Ended(Why::Closed));
      }
      SessionEvent::Disconnected { .. } => {
        self.tracker.abandon();
        return Some(Exit::Ended(Why::Closed));
      }
    }
    None
  }

  /// The client's `exec`, where its repository lives (the module doc): on another shard, the connection moves there with
  /// the `exec` undecided; here, it is dispatched now. Only a connection whose key proved a principal moves: an `exec`
  /// without one is refused where it came.
  fn located(&mut self, command: Asked, git_protocol: Option<Vec<u8>>) -> Option<Exit> {
    if self.principal.is_some()
      && let Located::Elsewhere(shard) = self.site.locate(&command)
    {
      return Some(Exit::Moved {
        shard,
        exec: Exec {
          command,
          git_protocol,
        },
      });
    }
    self.on_exec(&command, git_protocol.as_deref());
    None
  }

  /// The client's `exec`: dispatched by the site for the proven principal, and accepted; run, or refused as its tool
  /// refuses. The session raises none before a proof; one without a principal is refused as a failed request.
  fn on_exec(&mut self, command: &Asked, git_protocol: Option<&[u8]>) {
    let Some(principal) = self.principal.as_ref() else {
      let _ = self.session.answer_exec(false);
      return;
    };
    let dispatched = self.site.dispatch(principal, command, git_protocol);
    if self.session.answer_exec(true).is_err() {
      return;
    }
    self.running = Some(match dispatched {
      Dispatched::Run(command) => Running::started(command),
      Dispatched::Refuse(refused) => {
        let _ = self.state.with(|state| {
          state.count(|counts| counts.commands_refused = counts.commands_refused.saturating_add(1));
        });
        Running::refused(refused)
      }
    });
    self.enter(Phase::Running);
  }

  /// The client's input: held by the command until a step consumes it; given back to the client's window at once when
  /// no command takes it (a refusal, an exit), as it is dropped.
  async fn on_data(&mut self, run: &[u8]) -> Option<Why> {
    let taken = self
      .running
      .as_mut()
      .and_then(Running::taking)
      .map(|command| command.input(run));
    match taken {
      Some(Ok(())) => {
        if let Some(running) = self.running.as_mut() {
          running.next = Next::More;
        }
      }
      Some(Err(refused)) => self.fail(refused),
      None => {
        if self.session.consumed(run.len()).is_err() {
          return Some(Why::Refused);
        }
      }
    }
    yield_now().await;
    None
  }

  /// The client's input ended.
  fn on_eof(&mut self) {
    if let Some(running) = self.running.as_mut()
      && let Some(command) = running.taking()
    {
      command.input_end();
      running.next = Next::More;
    }
  }

  /// The command failed: its words are sent, and it exits with its status.
  fn fail(&mut self, refused: Refused) {
    if let Some(running) = self.running.as_mut() {
      *running = Running::refused(refused);
    }
  }

  /// One turn of the command: what it wrote handed to the channel, a step asked of it, or its exit sent. A command that
  /// says it consumed input it was never given ends the connection.
  async fn turn(&mut self) -> Result<Turn, Why> {
    let Some(running) = self.running.as_mut() else {
      return Ok(Turn::Idle);
    };
    if running.held {
      return Ok(Turn::Client);
    }
    if running.unsent() {
      return Ok(self.offer());
    }
    let next = running.next;
    match next {
      Next::Exited(status) => Ok(self.exit(status)),
      Next::Input => Ok(Turn::Client),
      Next::More => self.step().await,
    }
  }

  /// Offers what the command wrote to the channel: standard error first, then standard output.
  fn offer(&mut self) -> Turn {
    let Some(running) = self.running.as_mut() else {
      return Turn::Idle;
    };
    let taken = match running.command.as_mut() {
      Some(command) => {
        let (stderr, stdout) = command.output();
        let taken_stderr = self.session.send_stderr(stderr).unwrap_or(0);
        let taken_stdout = if taken_stderr == stderr.len() {
          self.session.send_data(stdout).unwrap_or(0)
        } else {
          0
        };
        command.sent(taken_stderr, taken_stdout);
        taken_stderr.saturating_add(taken_stdout)
      }
      None => {
        let taken = self.session.send_stderr(&running.refusal).unwrap_or(0);
        running.refusal.drain(..taken.min(running.refusal.len()));
        taken
      }
    };
    if taken == 0 {
      running.held = true;
      return Turn::Client;
    }
    Turn::Moved
  }

  /// Steps the command and gives the input it consumed back to the client's window, then yields: one step a turn.
  async fn step(&mut self) -> Result<Turn, Why> {
    let Some(command) = self
      .running
      .as_mut()
      .and_then(|running| running.command.as_mut())
    else {
      // A command-less run (a refusal, a failure) always has its exit queued, so it is never stepped.
      return Ok(Turn::Idle);
    };
    match command.step().await {
      Ok(stepped) => {
        if let Some(running) = self.running.as_mut() {
          running.next = stepped.next;
        }
        if stepped.consumed > 0 && self.session.consumed(stepped.consumed).is_err() {
          return Err(Why::Refused);
        }
      }
      Err(refused) => self.fail(refused),
    }
    yield_now().await;
    Ok(Turn::Moved)
  }

  /// The command exited with `status` and its output is sent: `exit-status`, EOF and close, and the connection closes.
  fn exit(&mut self, status: u32) -> Turn {
    if self.session.finish(status).is_err() {
      return Turn::Idle;
    }
    let refused = self
      .running
      .take()
      .is_some_and(|running| running.command.is_none());
    if !refused {
      let _ = self.state.with(|state| {
        state.count(|counts| counts.commands_run = counts.commands_run.saturating_add(1));
      });
    }
    self.enter(Phase::Closing);
    Turn::Moved
  }
}

/// Serves one connection until it ends or moves to its repository's shard (the module doc's loop), its seat here given
/// back however it leaves; `make` makes its site here, and there.
async fn drive<S, M>(stream: TcpStream, make: M, seat: Seat, tracker: Tracker)
where
  S: SshSite + 'static,
  S::Keys: Send + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let accepted_ns = now_ns();
  let mut site = make();
  let keys = site.keys();
  let Some(session) = seat
    .state
    .with(|state| state.session(keys, accepted_ns))
    .flatten()
  else {
    crate::fleet::count_refusal(NO_SESSION);
    let _ = seat
      .state
      .with(|state| state.count(|counts| counts.failed = counts.failed.saturating_add(1)));
    return;
  };
  let mut link = Link {
    stream,
    buffer: vec![0; READ_BYTES],
    unread: Vec::new(),
    session,
    site,
    state: seat.state,
    slot: seat.slot,
    tracker,
    phase: Phase::Handshake,
    principal: None,
    running: None,
    accepted_ns,
  };
  link.enter(Phase::Handshake);
  let exit = link.run().await;
  leave(link, seat, exit, make);
}

/// A connection's loop left as `exit` (the module doc): an end counted before its socket closes and its seat comes
/// back; or a move to its repository's shard, counted here, its seat here given back before the connection leaves.
fn leave<S, M>(link: Link<S>, seat: Seat, exit: Exit, make: M)
where
  S: SshSite + 'static,
  S::Keys: Send + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  match exit {
    Exit::Ended(why) => {
      ended(&link, why);
      drop(link);
      drop(seat);
    }
    Exit::Moved { shard, exec } => {
      let Link {
        stream,
        unread,
        session,
        state,
        tracker,
        phase,
        principal,
        accepted_ns,
        ..
      } = link;
      let _ =
        state.with(|state| state.count(|counts| counts.moved = counts.moved.saturating_add(1)));
      drop(seat);
      migrate::<S, M>(
        shard,
        Moving {
          stream: stream.into_owned(),
          session,
          unread,
          tracker,
          phase,
          principal,
          accepted_ns,
          exec,
          make,
        },
      );
    }
  }
}

/// A connection on its way to the shard its repository lives on (the module doc): its socket's handle, its session with
/// the client's bytes it has not taken, its waits, its phase, its principal, when it was accepted, its `exec`
/// undecided, and what makes its site there. Each part is plain data, so it moves whole.
struct Moving<K: KeyHolders, M> {
  /// The socket's handle, its readiness armed on no shard between awaits.
  stream: OwnedStream,
  /// The session.
  session: Session<SystemEntropy, K>,
  /// The client's bytes read and not yet taken by the session.
  unread: Vec<u8>,
  /// Its waits.
  tracker: Tracker,
  /// The phase it is in.
  phase: Phase,
  /// The principal its key proved.
  principal: Option<Principal>,
  /// When its listener accepted it.
  accepted_ns: u64,
  /// The `exec` dispatched there.
  exec: Exec,
  /// What makes its site.
  make: M,
}

/// Moves `moving` to runtime shard `shard`: one spawn on that shard's control channel carrying the connection, which
/// there is seated and served ([`arrived`]). A spawn the channel refuses dropped the socket's handle with it, so the
/// connection closed; it is counted, as the HTTP edge counts a refused move.
fn migrate<S, M>(shard: u16, moving: Moving<S::Keys, M>)
where
  S: SshSite + 'static,
  S::Keys: Send + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let arrive = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      arrived::<S, M>(moving);
    }),
    None,
  );
  if slates_rt::registry::send_control(shard, slates_rt::control::Control::Spawn(Box::new(arrive)))
    .is_err()
  {
    crate::fleet::count_refusal(MOVE_REFUSED);
  }
}

/// A connection arrived on this shard ([`migrate`]): served by a task of this shard's, which adopts its socket, seats
/// it ([`seat_arrival`]) and drives it on. Lost, and counted, on a shard keeping no edge state.
fn arrived<S, M>(moving: Moving<S::Keys, M>)
where
  S: SshSite + 'static,
  S::Keys: Send + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let Some(state) = here() else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return;
  };
  match slates_rt::futures::spawn(serve_arrival::<S, M>(state, moving)) {
    Ok(task) => {
      if detach(task).is_err() {
        crate::fleet::count_refusal(NOT_DETACHED);
      }
    }
    Err(_) => {
      crate::fleet::count_refusal(SPAWN_REFUSED);
    }
  }
}

/// Adopts the socket of a connection that moved here, seats it, dispatches its `exec` and serves it on: its task is the
/// seat's, so a reap here cancels it. Lost, and counted, where the socket cannot be adopted; closed, and counted, where
/// no seat can be had.
async fn serve_arrival<S, M>(state: Kept<EdgeState>, moving: Moving<S::Keys, M>)
where
  S: SshSite + 'static,
  S::Keys: Send + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let Moving {
    stream,
    session,
    unread,
    tracker,
    phase,
    principal,
    accepted_ns,
    exec,
    make,
  } = moving;
  let Ok(stream) = TcpStream::from_owned(stream) else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return;
  };
  let Some(seat) = seated(state).await else {
    return;
  };
  let mut link = Link {
    stream,
    buffer: vec![0; READ_BYTES],
    unread,
    session,
    site: make(),
    state,
    slot: seat.slot,
    tracker,
    phase,
    principal,
    running: None,
    accepted_ns,
  };
  link.on_exec(&exec.command, exec.git_protocol.as_deref());
  let exit = link.run().await;
  leave(link, seat, exit, make);
}

/// A seat here for a connection that moved here, its task recorded in it and its arrival counted; none, counted by why,
/// when none can be had ([`seat_arrival`]) or the edge's state left this shard.
async fn seated(state: Kept<EdgeState>) -> Option<Seat> {
  let slot = match seat_arrival(state).await {
    Ok(slot) => slot,
    Err(ArrivalRefused::Full) => {
      let _ = state.with(|state| {
        state.count(|counts| counts.arrivals_refused = counts.arrivals_refused.saturating_add(1));
      });
      crate::fleet::count_refusal(ARRIVAL_REFUSED);
      return None;
    }
    Err(ArrivalRefused::Gone) => {
      crate::fleet::count_refusal(ARRIVAL_LOST);
      return None;
    }
  };
  let Some(seat) = Seat::take(state, slot) else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return None;
  };
  let _ = state.with(|state| {
    if let Some(task) = slates_rt::futures::current_task() {
      state.set_task(slot, task);
    }
    state.count(|counts| counts.arrived = counts.arrived.saturating_add(1));
  });
  Some(seat)
}

/// Why a connection that moved here was not seated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArrivalRefused {
  /// Every seat is taken and as many arrivals already wait as the shard has seats.
  Full,
  /// The edge's state is gone.
  Gone,
}

/// A seat for a connection that moved here (the module doc): a free one at once; with every seat taken, as the listener
/// seats a new connection ([`seat_for`]: the most overdue connection's, or the next to come back), while fewer arrivals
/// wait than the shard has seats; refused past that.
async fn seat_arrival(state: Kept<EdgeState>) -> Result<usize, ArrivalRefused> {
  let free = state
    .with(|state| state.with_seats(|seats| seats.iter().position(Option::is_none)))
    .ok_or(ArrivalRefused::Gone)?
    .flatten();
  if let Some(slot) = free {
    return Ok(slot);
  }
  if !state
    .with(EdgeState::admit_arrival)
    .ok_or(ArrivalRefused::Gone)?
  {
    return Err(ArrivalRefused::Full);
  }
  let _waiting = Arriving(state);
  seat_for(state).await.map_err(|_| ArrivalRefused::Gone)
}

/// A connection that moved here, counted among the arrivals waiting for a seat for as long as it waits, however its wait
/// ends: seated, refused, or cancelled with its task.
struct Arriving(Kept<EdgeState>);

impl Drop for Arriving {
  fn drop(&mut self) {
    // Off its shard (the runtime gone) no edge is left to count it in.
    let _ = self.0.with(EdgeState::arrival_seated);
  }
}

/// Counts how a connection ended, and its lifetime when it was proven and ended as its client meant. Called before its
/// socket closes, so its client cannot see the end before the count.
fn ended<S: SshSite>(link: &Link<S>, why: Why) {
  if why == Why::Failed {
    crate::fleet::count_refusal(SOCKET_FAILED);
  }
  let lifetime_ns = now_ns().saturating_sub(link.accepted_ns);
  let proven = link.tracker.proven();
  let _ = link.state.with(|state| {
    state.count(|counts| match why {
      Why::Failed => counts.failed = counts.failed.saturating_add(1),
      Why::Refused => counts.refused = counts.refused.saturating_add(1),
      Why::Closed => {}
    });
    if proven
      && why == Why::Closed
      && let Ok(mut lifetimes) = state.lifetimes.try_borrow_mut()
    {
      lifetimes.on_sample(lifetime_ns);
    }
  });
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../ssh/src/tests/client.rs"]
mod ssh_client;

#[cfg(test)]
#[path = "vcs_ssh/tests/serve.rs"]
mod tests;
