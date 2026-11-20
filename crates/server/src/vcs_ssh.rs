//! The version-control servers' SSH edge on the daemon's runtime (A-651; §4.17.2 "Transports" item 2 and "Pieces (git)"
//! piece 9; §4.17.3's SSH transport; §4.17.4's svn+ssh, piece 12; §4.3, R6): a listener on the runtime's TCP, one
//! connection a task, each driving `slates-ssh`'s [`Session`] (A-594, A-633, A-634) over its socket; every wait on a client
//! learned per phase ([`waits`]); the connections served at once and each one's room derived from the shard's budget
//! (A-625 part 2's [`Bounds`]); and a site ([`site`]) that proves keys to principals of §4.13 and dispatches each `exec`
//! by its tool. It is the SSH counterpart of the HTTP edge (`crate::vcs_http`), and keeps its shape.
//!
//! **The listener** ([`serve`]) is a task on its shard and owns its connections: each is a child task, detached so its
//! slot is given back as soon as it ends, and cancelled and joined when the listener ends, whoever ends it, which is the
//! runtime's rule for a task's children. So no connection outlives its listener (banned item 9).
//!
//! **Bounds, derived** (A-625 part 2's [`Bounds::of_shard`]). An SSH connection is a client of its shard: the edge serves
//! as many at once as the shard seats clients, and each may hold what one client's region holds, which is its channel's
//! window (`SessionConfig::channel_window`): the client's bytes the server holds unconsumed. Beside it a connection holds
//! one read ([`READ_BYTES`]) and the session's own buffers, each about a packet cap. The packet cap is the one every peer
//! accepts ([`PACKET_CAP`]). At the bound the listener stops accepting until a seat is free, and the kernel's queue of
//! connections not yet accepted is the same bound (`Bounds::backlog`).
//!
//! **Overdue and reaped** (main's ruling, 2026-11-19). A wait past its learned bound ([`waits`]) only marks its connection
//! overdue. While every seat is taken, which is while the listener waits for one, the listener reaps the most overdue
//! connection: it cancels its task, whose seat then comes back, and a new connection takes it. A reap is no sample
//! (Karn's rule) and backs its phase off as a cut does, for a proven connection only. Under no pressure nothing is cut,
//! because an SSH connection waits on people inside it: OpenSSH asks for a key's passphrase only after the server's
//! `SSH_MSG_USERAUTH_PK_OK` (`input_userauth_pk_ok` signs through `load_identity_file`, which prompts) \[C: OpenSSH
//! `sshconnect2.c`\], asks a person to confirm an unknown host key before its `SSH_MSG_NEWKEYS` (`check_host_key` in
//! `sshconnect.c`) \[C\], and a push's client compresses its pack before it sends a byte of it. Cutting such a wait at a
//! bound learned from the clients that finished would cut every agent-less passphrase on every connection, and a cut
//! before the proof backs nothing off, so it would never be learned. Reaping only for a seat someone wants is the
//! rent-or-buy choice: a stall costs its seat, and is paid for only when the seat is wanted.
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
//! **Owed** (GAPS, the §4.17 entry): the daemon starting an edge (its address, the host key's keeping, the key store
//! over §4.13's principals, the repositories' lookup on their partitions); the four services' server halves, each refused
//! as not served until it lands (git's upload-pack, A-640; receive-pack, A-620; Mercurial's `serve --stdio`, A-650;
//! Subversion's tunnel, §4.17.4 piece 12); the live `git clone` over SSH (main's run).

pub(crate) mod site;
mod waits;

use std::cell::{Cell, RefCell};
use std::task::{Poll, Waker};

use slates_cluster::failover::RunningDuration;
use slates_db::catalog::Principal;
use slates_rt::TaskId;
use slates_rt::error::RtError;
use slates_rt::futures::{cancel, detach, now_ns, spawn_child, within, yield_now};
use slates_rt::shard::Kept;
use slates_rt::tcp::{TcpListener, TcpStream};
use slates_ssh::entropy::SystemEntropy;
use slates_ssh::hostkey::{ED25519_SEED_LEN, HostKey};
use slates_ssh::keylife::{UsageCap, key_time_limit_ns};
use slates_ssh::packet::MAX_PACKET_EVERY_PEER_ACCEPTS;
use slates_ssh::session::{Session, SessionConfig, SessionEvent};
use slates_ssh::transport::TransportConfig;
use slates_ssh::userauth::RECOMMENDED_FAILURE_LIMIT;

use crate::vcs_http::Bounds;
use site::{Command, Dispatched, Next, Refused, SshSite};
use waits::{Phase, PhaseWaits, Reap, SeatView, Tracker, choose};

/// Derived: the transport's packet cap: the size every peer accepts (RFC 4253 §6.1: "All implementations MUST be able to
/// process packets with an uncompressed payload length of 32768 bytes or less and a total packet size of 35000 bytes or
/// less"). A larger cap buys nothing from OpenSSH, whose session channels carry packets of 32 KiB (`CHAN_SES_PACKET_DEFAULT`
/// in `channels.h`) \[C\], and every byte past it is held per connection.
const PACKET_CAP: usize = MAX_PACKET_EVERY_PEER_ACCEPTS;
/// Derived: the bytes one read takes from a connection's socket: a packet cap, so a read never splits a packet it could
/// have taken whole.
const READ_BYTES: usize = PACKET_CAP;

/// Format: the refusal counts the edge keeps in the daemon's status report beside its own [`Counts`] (banned item 9:
/// every connection ended other than by its client is counted by its reason).
const SPAWN_REFUSED: &str = "vcs_ssh.spawn";
/// A connection task that could not be detached, which keeps its slot until its listener ends.
const NOT_DETACHED: &str = "vcs_ssh.detach";
/// A connection reaped for its seat.
const REAPED: &str = "vcs_ssh.reaped";
/// A reaped connection whose task was already gone.
const REAP_MISSED: &str = "vcs_ssh.reap_missed";
/// A connection no session could be made for.
const NO_SESSION: &str = "vcs_ssh.no_session";
/// A connection whose socket failed.
const SOCKET_FAILED: &str = "vcs_ssh.socket";

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
  /// Times the listener waited for a seat.
  pub(crate) full: u64,
  /// Connections reaped for their seats.
  pub(crate) reaped: u64,
  /// Waits that completed past their bound because a seat was free.
  pub(crate) late: u64,
  /// Connections whose socket failed or for which no session could be made.
  pub(crate) failed: u64,
  /// Connections the session refused (a protocol error, a failed exchange, too many authentication failures).
  pub(crate) refused: u64,
  /// `exec`s refused by the site.
  pub(crate) commands_refused: u64,
  /// Commands run to their exit.
  pub(crate) commands_run: u64,
}

/// An edge's state on its shard, shared by its listener and its connections through a [`Kept`] handle: what bounds it,
/// its host key, its seats, the listener waiting for one, every phase's learned waits, the connections' lifetimes, and
/// its counts. Nothing in it is lent across a wait.
pub(crate) struct EdgeState {
  /// The edge's bounds.
  bounds: Bounds,
  /// The host key's seed: each connection's key is made from it.
  host_seed: HostSeed,
  /// Each seat, as the reaper sees it; as many as the bounds' connections.
  seats: RefCell<Vec<SeatView>>,
  /// The seats free.
  free: RefCell<Vec<usize>>,
  /// The listener, when it waits for a seat.
  waiting: RefCell<Option<Waker>>,
  /// Every phase's learned waits.
  waits: RefCell<PhaseWaits>,
  /// How long the authenticated connections lived.
  lifetimes: RefCell<RunningDuration>,
  /// The counts.
  counts: Cell<Counts>,
}

impl EdgeState {
  /// An edge's state: `bounds`, and the seed of its Ed25519 host key.
  pub(crate) fn new(bounds: Bounds, host_seed: [u8; ED25519_SEED_LEN]) -> EdgeState {
    let seats = bounds.connections;
    EdgeState {
      bounds,
      host_seed: HostSeed(host_seed),
      seats: RefCell::new(vec![SeatView::default(); seats]),
      free: RefCell::new((0..seats).rev().collect()),
      waiting: RefCell::new(None),
      waits: RefCell::new(PhaseWaits::default()),
      lifetimes: RefCell::new(RunningDuration::new()),
      counts: Cell::new(Counts::default()),
    }
  }

  /// What the edge has counted.
  pub(crate) fn counts(&self) -> Counts {
    self.counts.get()
  }

  /// The connections being served.
  pub(crate) fn serving(&self) -> usize {
    self
      .seats
      .try_borrow()
      .map_or(0, |seats| seats.iter().filter(|seat| seat.taken).count())
  }

  /// How many of `phase`'s waits completed and how many were reaped after a proof; nothing while the waits are lent,
  /// which is never across a wait.
  pub(crate) fn learned(&self, phase: Phase) -> (u64, u64) {
    self
      .waits
      .try_borrow()
      .map_or((0, 0), |waits| (waits.samples(phase), waits.cuts(phase)))
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

  /// A session for a connection accepted at `now_ns` whose keys' lookup is `keys`; none when none can be made.
  fn session<K: slates_ssh::userauth::KeyHolders>(
    &self,
    keys: K,
    now_ns: u64,
  ) -> Option<Session<SystemEntropy, K>> {
    let host_key = HostKey::ed25519_from_seed(&self.host_seed.0).ok()?;
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

  /// Takes a free seat; none when every seat is taken.
  fn take_seat(&self) -> Option<usize> {
    let index = self.free.try_borrow_mut().ok()?.pop()?;
    let mut seats = self.seats.try_borrow_mut().ok()?;
    let seat = seats.get_mut(index)?;
    *seat = SeatView {
      taken: true,
      ..SeatView::default()
    };
    Some(index)
  }

  /// Gives seat `index` back, waking the listener if it waits for one.
  fn give_back(&self, index: usize) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(seat) = seats.get_mut(index)
    {
      *seat = SeatView::default();
    }
    if let Ok(mut free) = self.free.try_borrow_mut() {
      free.push(index);
    }
    let waiting = self
      .waiting
      .try_borrow_mut()
      .ok()
      .and_then(|mut waiting| waiting.take());
    if let Some(waker) = waiting {
      waker.wake();
    }
  }

  /// Records seat `index`'s task.
  fn assign(&self, index: usize, task: TaskId) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(seat) = seats.get_mut(index)
    {
      seat.task = Some(task);
    }
  }

  /// Publishes seat `index`'s connection's wait on its client, and whether it is proven.
  fn publish(&self, index: usize, tracker: &Tracker) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(seat) = seats.get_mut(index)
    {
      seat.waiting = tracker.waiting();
      seat.proven = tracker.proven();
    }
  }

  /// The listener's next step at `now_ns` (the module doc): a free seat, a connection to reap, or a wait, its waker kept
  /// to be woken when a seat comes back; the wait counted once (`first`).
  fn seat_step(&self, now_ns: u64, waker: &Waker, first: bool) -> SeatStep {
    if self.free.try_borrow().is_ok_and(|free| !free.is_empty()) {
      return SeatStep::Free;
    }
    if let Ok(mut waiting) = self.waiting.try_borrow_mut() {
      *waiting = Some(waker.clone());
    }
    if first {
      self.count(|counts| counts.full = counts.full.saturating_add(1));
    }
    let chosen = match (self.seats.try_borrow(), self.waits.try_borrow()) {
      (Ok(seats), Ok(waits)) => choose(&seats, &waits, now_ns),
      _ => Reap::Never,
    };
    match chosen {
      Reap::Seat(index) => self
        .reap(index)
        .map_or(SeatStep::Wait(None), SeatStep::Reap),
      Reap::Until(deadline_ns) => SeatStep::Wait(Some(deadline_ns)),
      Reap::Never => SeatStep::Wait(None),
    }
  }

  /// Marks seat `index` reaped, backs its phase off if its connection is proven, counts it, and gives its task to
  /// cancel.
  fn reap(&self, index: usize) -> Option<TaskId> {
    let mut seats = self.seats.try_borrow_mut().ok()?;
    let seat = seats.get_mut(index)?;
    seat.reaped = true;
    let task = seat.task?;
    if seat.proven
      && let Some(waiting) = seat.waiting
    {
      self.with_waits(|waits| waits.ran_out(waiting.phase));
    }
    self.count(|counts| counts.reaped = counts.reaped.saturating_add(1));
    Some(task)
  }
}

/// What the listener does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeatStep {
  /// A seat is free: accept.
  Free,
  /// Reap this task's connection, then wait for its seat.
  Reap(TaskId),
  /// Wait for a seat, until this instant when one may become overdue.
  Wait(Option<u64>),
}

/// A connection's seat among those its edge serves at once: taken as it is accepted and given back when this drops,
/// however the connection ends (its loop's end, its task's cancellation by a reap or with its listener, a refused
/// spawn), the listener woken if it waits for one.
struct Seat {
  /// The edge.
  state: Kept<EdgeState>,
  /// Which seat.
  index: usize,
}

impl Seat {
  /// Takes a seat on the edge `state` keeps; none off its shard, or when none is free.
  fn take(state: Kept<EdgeState>) -> Option<Seat> {
    let index = state.with(EdgeState::take_seat).flatten()?;
    Some(Seat { state, index })
  }
}

impl Drop for Seat {
  fn drop(&mut self) {
    let index = self.index;
    // Off its shard (the runtime gone) no edge is left to give the seat back to.
    let _ = self.state.with(|state| state.give_back(index));
  }
}

/// Waits until a seat is free, reaping the most overdue connection while none is (the module doc).
async fn seat_free(state: Kept<EdgeState>) -> Result<(), EdgeRefused> {
  let mut first = true;
  loop {
    let now = now_ns();
    let step = std::future::poll_fn(|cx| {
      let step = state.with(|state| state.seat_step(now, cx.waker(), first));
      Poll::Ready(step.ok_or(EdgeRefused::StateGone))
    })
    .await?;
    first = false;
    match step {
      SeatStep::Free => return Ok(()),
      SeatStep::Reap(task) => {
        crate::fleet::count_refusal(REAPED);
        if cancel(task).is_err() {
          crate::fleet::count_refusal(REAP_MISSED);
        }
        seat_given_back(state).await?;
      }
      SeatStep::Wait(None) => seat_given_back(state).await?,
      SeatStep::Wait(Some(deadline_ns)) => {
        within(deadline_ns.saturating_sub(now), seat_given_back(state))
          .await
          .map_err(|_| EdgeRefused::StateGone)?
          .transpose()?;
      }
    }
  }
}

/// Waits until a seat comes back: ready at once when one is free, else when the listener's waker is woken by one.
async fn seat_given_back(state: Kept<EdgeState>) -> Result<(), EdgeRefused> {
  let mut asked = false;
  std::future::poll_fn(|cx| {
    let free = state.with(|state| {
      if state.free.try_borrow().is_ok_and(|free| !free.is_empty()) {
        return true;
      }
      if let Ok(mut waiting) = state.waiting.try_borrow_mut() {
        *waiting = Some(cx.waker().clone());
      }
      false
    });
    match free {
      Some(true) => Poll::Ready(Ok(())),
      // Woken once its waker was kept: a seat came back, or the reaper's deadline passed (checked by the caller).
      Some(false) if asked => Poll::Ready(Ok(())),
      Some(false) => {
        asked = true;
        Poll::Pending
      }
      None => Poll::Ready(Err(EdgeRefused::StateGone)),
    }
  })
  .await
}

/// Serves the edge `state` keeps on `listener` until the listener fails or its task ends (the module doc): each
/// connection a child task, its keys and commands those of a site `site` makes for it. Refused at once when `state` is
/// not on this shard.
pub(crate) async fn serve<S, M>(
  listener: TcpListener,
  state: Kept<EdgeState>,
  mut site: M,
) -> Result<(), EdgeRefused>
where
  S: SshSite + 'static,
  M: FnMut() -> S,
{
  state.with(|_| ()).ok_or(EdgeRefused::StateGone)?;
  loop {
    seat_free(state).await?;
    let stream = listener.accept().await.map_err(EdgeRefused::Listener)?;
    let _ =
      state.with(|state| state.count(|counts| counts.accepted = counts.accepted.saturating_add(1)));
    let Some(seat) = Seat::take(state) else {
      // No seat after one was free: the state left its shard.
      return Err(EdgeRefused::StateGone);
    };
    let index = seat.index;
    match spawn_child(drive(stream, site(), seat)) {
      Ok(task) => {
        let _ = state.with(|state| state.assign(index, task));
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

/// A running command and what it has written that the channel has not yet taken.
struct Running<C> {
  /// The command; none for a refused `exec`, which only writes its refusal.
  command: Option<C>,
  /// Standard output not yet sent.
  out: Vec<u8>,
  /// Standard error not yet sent.
  err: Vec<u8>,
  /// What it waits for.
  next: Next,
  /// Whether the channel took less than it was offered: held until the session says it is writable.
  held: bool,
}

impl<C> Running<C> {
  /// A command just started.
  fn started(command: C) -> Self {
    Self {
      command: Some(command),
      out: Vec::new(),
      err: Vec::new(),
      next: Next::More,
      held: false,
    }
  }

  /// A refused `exec`: its message, then its status.
  fn refused(refused: Refused) -> Self {
    Self {
      command: None,
      out: Vec::new(),
      err: refused.stderr,
      next: Next::Exited(refused.exit_status),
      held: false,
    }
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

/// One connection: its socket, its session, its site, its waits, the principal its key proved, and its command.
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
  seat: usize,
  /// The connection's waits.
  tracker: Tracker,
  /// The phase it is in.
  phase: Phase,
  /// The principal its key proved.
  principal: Option<Principal>,
  /// Its command, once its `exec` was dispatched.
  running: Option<Running<S::Command>>,
}

impl<S: SshSite> Link<S> {
  /// Runs `f` over the connection's waits and the edge's learned ones, then publishes the connection's wait to its
  /// seat; none off the edge's shard.
  fn learning<R>(&mut self, f: impl FnOnce(&mut Tracker, &mut PhaseWaits) -> R) -> Option<R> {
    let seat = self.seat;
    let tracker = &mut self.tracker;
    self.state.with(|state| {
      let result = state.with_waits(|waits| f(&mut *tracker, waits));
      state.publish(seat, &*tracker);
      result
    })?
  }

  /// The connection moves into `phase` now.
  fn enter(&mut self, phase: Phase) {
    let now = now_ns();
    self.phase = phase;
    let _ = self.learning(|tracker, waits| tracker.enter(phase, now, waits));
  }

  /// The running command's silence begins now, if one runs.
  fn silence(&mut self) {
    if self.phase == Phase::Running {
      let now = now_ns();
      let _ = self.learning(|tracker, _| tracker.silence_began(now));
    }
  }

  /// The running command's silence ended now.
  fn heard(&mut self) {
    let now = now_ns();
    let _ = self.learning(|tracker, waits| tracker.silence_ended(now, waits));
  }

  /// Runs the connection until it ends (the module doc's loop).
  async fn run(&mut self) -> Why {
    loop {
      if !self.session.pending_output().is_empty() {
        if let Err(why) = self.write().await {
          return why;
        }
        continue;
      }
      if self.session.is_closed() {
        return Why::Refused;
      }
      if let Some(event) = self.session.poll_event() {
        match self.on_event(event).await {
          Some(why) => return why,
          None => continue,
        }
      }
      if !self.unread.is_empty() && self.feed() {
        continue;
      }
      match self.turn().await {
        Turn::Moved => continue,
        Turn::Client => self.silence(),
        Turn::Idle => {}
      }
      if let Err(why) = self.read().await {
        return why;
      }
    }
  }

  /// Writes what the session has to send: a silence of the running command while the client's room is waited for.
  async fn write(&mut self) -> Result<(), Why> {
    self.silence();
    let bytes = self.session.pending_output().to_vec();
    if self.stream.write_all(&bytes).await.is_err() {
      self.tracker.abandon();
      return Err(Why::Failed);
    }
    self.heard();
    self
      .session
      .consume_output(bytes.len())
      .map_err(|_| Why::Refused)
  }

  /// Reads the client's bytes into the session, within the session's own timer; the client's close ends the loop.
  async fn read(&mut self) -> Result<(), Why> {
    let started_ns = now_ns();
    let read = self.stream.read(&mut self.buffer);
    let outcome = match self.session.next_timer_ns() {
      Some(timer_ns) => within(timer_ns.saturating_sub(started_ns), read).await,
      None => Ok(Some(read.await)),
    };
    match outcome {
      Ok(Some(Ok(0))) => {
        // The client left: a closing connection's wait ends as it should; any other wait's true length is unknown.
        if self.phase == Phase::Closing {
          let now = now_ns();
          let _ = self.learning(|tracker, waits| tracker.end(now, waits));
        } else {
          self.tracker.abandon();
        }
        Err(Why::Closed)
      }
      Ok(Some(Ok(count))) => {
        self.heard();
        self
          .unread
          .extend_from_slice(self.buffer.get(..count).unwrap_or_default());
        self.feed();
        Ok(())
      }
      Ok(None) => self.session.poll_timers(now_ns()).map_err(|_| Why::Refused),
      Ok(Some(Err(_))) | Err(_) => {
        self.tracker.abandon();
        Err(Why::Failed)
      }
    }
  }

  /// Hands the unread bytes to the session: whether it took any. The handshake ends when they established the transport.
  fn feed(&mut self) -> bool {
    let taken = match self.session.receive(&self.unread, now_ns()) {
      Ok(taken) => taken,
      Err(_) => {
        // The session refused the client and queued its disconnect: nothing more of the client's is read.
        self.unread.clear();
        return true;
      }
    };
    self.unread.drain(..taken.min(self.unread.len()));
    if self.phase == Phase::Handshake && self.session.transport().is_established() {
      self.enter(Phase::Authentication);
    }
    taken > 0
  }

  /// Takes one session event: the end of the loop when it ends the connection.
  async fn on_event(&mut self, event: SessionEvent<Principal>) -> Option<Why> {
    match event {
      SessionEvent::Authenticated(authenticated) => {
        self.principal = Some(authenticated.principal);
        let _ = self.learning(|tracker, waits| tracker.prove(waits));
        self.enter(Phase::Setup);
      }
      SessionEvent::Exec {
        command,
        git_protocol,
      } => self.on_exec(&command, git_protocol.as_deref()),
      SessionEvent::Data(run) => return self.on_data(&run).await,
      SessionEvent::Eof => self.on_eof().await,
      SessionEvent::Writable => {
        if let Some(running) = self.running.as_mut() {
          running.held = false;
        }
      }
      SessionEvent::ChannelClosed => {
        let now = now_ns();
        let _ = self.learning(|tracker, waits| tracker.end(now, waits));
        return Some(Why::Closed);
      }
      SessionEvent::Disconnected { .. } => {
        self.tracker.abandon();
        return Some(Why::Closed);
      }
    }
    None
  }

  /// The client's `exec`: dispatched by the site for the proven principal, and accepted; run, or refused as its tool
  /// refuses. The session raises none before a proof; one without a principal is refused as a failed request.
  fn on_exec(&mut self, command: &slates_ssh::command::Command, git_protocol: Option<&[u8]>) {
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

  /// The client's input: to the command, then given back to the client's window; one run a turn.
  async fn on_data(&mut self, run: &[u8]) -> Option<Why> {
    let taken = match self.running.as_mut() {
      Some(Running {
        command: Some(command),
        next,
        ..
      }) if !matches!(next, Next::Exited(_)) => {
        let taken = command.input(run).await;
        *next = Next::More;
        taken
      }
      _ => Ok(()),
    };
    if let Err(refused) = taken {
      self.fail(refused);
    }
    if self.session.consumed(run.len()).is_err() {
      return Some(Why::Refused);
    }
    yield_now().await;
    None
  }

  /// The client's input ended.
  async fn on_eof(&mut self) {
    let ended = match self.running.as_mut() {
      Some(Running {
        command: Some(command),
        next,
        ..
      }) if !matches!(next, Next::Exited(_)) => {
        let ended = command.input_end().await;
        *next = Next::More;
        ended
      }
      _ => Ok(()),
    };
    if let Err(refused) = ended {
      self.fail(refused);
    }
  }

  /// The command failed: it writes its refusal and exits with its status.
  fn fail(&mut self, refused: Refused) {
    if let Some(running) = self.running.as_mut() {
      running.command = None;
      running.err.extend_from_slice(&refused.stderr);
      running.next = Next::Exited(refused.exit_status);
    }
  }

  /// One turn of the command: what it wrote handed to the channel, a new run asked of it, or its exit sent.
  async fn turn(&mut self) -> Turn {
    let Some(running) = self.running.as_mut() else {
      return Turn::Idle;
    };
    if running.held {
      return Turn::Client;
    }
    if !running.err.is_empty() || !running.out.is_empty() {
      return self.offer();
    }
    let next = running.next;
    match next {
      Next::Exited(status) => self.exit(status),
      Next::Input => Turn::Client,
      Next::More => self.ask().await,
    }
  }

  /// Offers the command's unsent output to the channel: standard error first, then standard output.
  fn offer(&mut self) -> Turn {
    let Some(running) = self.running.as_mut() else {
      return Turn::Idle;
    };
    let taken_err = self.session.send_stderr(&running.err).unwrap_or(0);
    running.err.drain(..taken_err.min(running.err.len()));
    let taken_out = if running.err.is_empty() {
      self.session.send_data(&running.out).unwrap_or(0)
    } else {
      0
    };
    running.out.drain(..taken_out.min(running.out.len()));
    if taken_err == 0 && taken_out == 0 {
      running.held = true;
      return Turn::Client;
    }
    Turn::Moved
  }

  /// Asks the command for its next run, then yields: one run a turn.
  async fn ask(&mut self) -> Turn {
    let Some(running) = self.running.as_mut() else {
      return Turn::Idle;
    };
    // A command-less run (a refusal, a failure) always has its exit queued, so it is never asked.
    let Some(command) = running.command.as_mut() else {
      return Turn::Idle;
    };
    match command.output(&mut running.out, &mut running.err).await {
      Ok(next) => running.next = next,
      Err(refused) => self.fail(refused),
    }
    yield_now().await;
    Turn::Moved
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

/// Serves one connection until it ends (the module doc's loop), its seat given back however it ends.
async fn drive<S: SshSite>(stream: TcpStream, mut site: S, seat: Seat) {
  let accepted_ns = now_ns();
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
    seat: seat.index,
    tracker: Tracker::default(),
    phase: Phase::Handshake,
    principal: None,
    running: None,
  };
  link.enter(Phase::Handshake);
  let why = link.run().await;
  ended(&link, why, accepted_ns);
  drop(link);
  drop(seat);
}

/// Counts how a connection ended, and its lifetime when it was proven and ended as its client meant. Called before its
/// socket closes, so its client cannot see the end before the count.
fn ended<S: SshSite>(link: &Link<S>, why: Why, accepted_ns: u64) {
  if why == Why::Failed {
    crate::fleet::count_refusal(SOCKET_FAILED);
  }
  let late = link.tracker.late();
  let lifetime_ns = now_ns().saturating_sub(accepted_ns);
  let proven = link.tracker.proven();
  let _ = link.state.with(|state| {
    state.count(|counts| {
      counts.late = counts.late.saturating_add(late);
      match why {
        Why::Failed => counts.failed = counts.failed.saturating_add(1),
        Why::Refused => counts.refused = counts.refused.saturating_add(1),
        Why::Closed => {}
      }
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
