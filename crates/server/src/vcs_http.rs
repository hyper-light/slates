//! The version-control servers' HTTP edge on the daemon's runtime (A-625 part 2; §4.17.2 "Transports" item 1 and
//! "Pieces (git)" piece 8; §4.3, R6): a listener on the runtime's TCP, one connection a task, each driving
//! `slates-http-edge`'s engine (A-625 part 1) over its socket; every wait on a client learned per phase ([`waits`]); the
//! connections served at once and each request's room derived from the shard's budget ([`Bounds`]); and a site that
//! decides each request ([`Site`]): git's ([`git`]) first, Mercurial's (§4.17.3) and Subversion's (§4.17.4) on the same
//! edge.
//!
//! **The listener** ([`serve`]) is a task on its shard and owns its connections: each is a child task
//! (`slates_rt::futures::spawn_child`), detached so its slot is given back as soon as it ends, and cancelled and joined
//! when the listener ends, whoever ends it, which is the runtime's rule for a task's children. So no connection outlives
//! its listener (banned item 9). A cleartext edge is refused unless its listener is on loopback: Basic's password and a
//! bearer token would otherwise cross the network in the clear (RFC 7617 §4, RFC 6750 §5.3).
//!
//! **Bounds, derived** ([`Bounds::of_shard`]). An HTTP connection is a client of its shard, as the MCP edge counts one
//! (`slates_mcp::http`, AUD-29-23): the edge serves as many at once as the shard seats clients (AC-2.6's admission limit,
//! `DaemonConfig::clients_per_shard`), and each request may hold what one client's region holds
//! (`RegionGeometry::total_bytes`): its head, its decoded path and every inflated byte of its body are charged to that
//! room (`slates-http`'s admission). Together the connections' requests hold at most the client share of the shard's
//! reserve, the share the seats were derived from. Beside its room a connection holds one read's bytes ([`READ_BYTES`],
//! a TLS record whole) and its TLS session's buffers.
//!
//! **At the bound the listener stops accepting** until a connection ends, and the kernel's queue of connections not yet
//! accepted is the same bound ([`Bounds::backlog`]); past it the kernel refuses them. An edge that accepted a connection
//! it could not serve would have to answer it: over HTTPS only after a handshake, which takes the task and the buffers the
//! bound exists to limit; and a refusal closed with the client's bytes unread is a reset that can erase it (A-183). Each
//! time the listener waits for a seat is counted ([`Counts::full`]).
//!
//! **Waits** ([`waits`]): each read and write on a connection runs within its phase's learned bound, none before the
//! phase's first measurement, and only a connection whose credentials proved a principal teaches the bounds.
//!
//! **One connection's loop** asks the engine what it needs next and does it: writes its bytes, reads within the phase's
//! wait, asks the site to decide a whole head (the answer says whether the credentials proved a principal), hands each run
//! of the body to the service the site started and then its end, passes the service's reply on, and half-closes and
//! drains for a staged close. One body run or one reply run a turn (§4.3): the task yields after each, so a large push or
//! clone never holds its shard, and a drain yields after each read it drops.
//!
//! **Owed** (GAPS, the §4.17 entry): the daemon starting an edge (its address, the operator's identity, an authenticator
//! over §4.13's principals and keys, the repositories' lookup on their partitions); the services' server halves (git's
//! upload-pack, A-615; receive-pack's commit, A-620 part 2), which answer `503` until they land; Mercurial's and
//! Subversion's sites; the MCP edge onto this engine; the live `git clone` over HTTPS (main's run).

mod git;
mod waits;

use std::cell::{Cell, RefCell};
use std::task::{Poll, Waker};

use slates_http::head::Head;
use slates_http::reply::Status;
use slates_http_edge::connection::{Connection, Decision, Ended, Fields, Next, Phase};
use slates_http_edge::tls::{HttpsSession, ServingIdentity};
use slates_rt::error::RtError;
use slates_rt::futures::{detach, now_ns, spawn_child, within, yield_now};
use slates_rt::shard::Kept;
use slates_rt::tcp::{SocketAddrV4, TcpListener, TcpStream};

use waits::{PhaseWaits, Tracker};

/// Format: the bytes one read takes from a connection's socket: one TLS record whole, its header and at most `2^14 + 256`
/// bytes of ciphertext (RFC 8446 §5.2: "The length MUST NOT exceed 2^14 + 256 bytes"), so a read never splits a record it
/// could have taken; a cleartext connection reads as much.
const READ_BYTES: usize = TLS_RECORD_HEADER + TLS_RECORD_PLAINTEXT + TLS_CIPHERTEXT_EXPANSION;
/// Format: a TLS record's header, its type, version and length (RFC 8446 §5.1).
const TLS_RECORD_HEADER: usize = 5;
/// Format: the most plaintext a TLS record carries (RFC 8446 §5.1: "MUST NOT exceed 2^14 bytes").
const TLS_RECORD_PLAINTEXT: usize = 1 << 14;
/// Format: the most a TLS record's protection adds (RFC 8446 §5.2).
const TLS_CIPHERTEXT_EXPANSION: usize = 256;

/// Format: the refusal counts the edge keeps in the daemon's status report beside its own [`Counts`] (banned item 9:
/// every connection ended other than by its client is counted by its reason).
const SPAWN_REFUSED: &str = "vcs_http.spawn";
/// A connection task that could not be detached, which keeps its slot until its listener ends.
const NOT_DETACHED: &str = "vcs_http.detach";
/// A connection no TLS session could be made for.
const NO_SESSION: &str = "vcs_http.no_session";
/// A connection ended because a wait ran past its learned bound.
const CUT: &str = "vcs_http.cut";
/// A connection whose socket failed.
const SOCKET_FAILED: &str = "vcs_http.socket";

/// What bounds an edge, derived from its shard's budget (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
  /// The connections served at once.
  pub(crate) connections: usize,
  /// The bytes each request may hold.
  pub(crate) room: u64,
}

impl Bounds {
  /// Derived: an edge on a shard that seats `clients_per_shard` clients, each over a region of `region_bytes`: one
  /// connection a seat, and each request one region's room (the module doc).
  pub(crate) fn of_shard(clients_per_shard: usize, region_bytes: usize) -> Bounds {
    Bounds {
      connections: clients_per_shard.max(1),
      room: u64::try_from(region_bytes).unwrap_or(u64::MAX),
    }
  }

  /// Derived: the listen backlog, the connections the kernel queues for the listener: as many as the edge serves at once
  /// (the module doc), clamped to what the call takes; the OS clamps it further to its own maximum.
  pub(crate) fn backlog(self) -> i32 {
    i32::try_from(self.connections).unwrap_or(i32::MAX)
  }
}

/// Whether a cleartext edge may serve on `address`: on loopback alone (the module doc).
fn admits_cleartext(address: SocketAddrV4) -> bool {
  address.ip().is_loopback()
}

/// Why an edge cannot serve, or stopped serving.
#[derive(Debug)]
pub(crate) enum EdgeRefused {
  /// A cleartext edge on a listener off loopback (the module doc).
  CleartextOffLoopback,
  /// The edge's state is not on the shard serving it: kept on another shard, or its shard gone.
  StateGone,
  /// The listener failed.
  Listener(RtError),
}

impl std::fmt::Display for EdgeRefused {
  /// The refusal as the daemon's log line names it.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      EdgeRefused::CleartextOffLoopback => formatter
        .write_str("a cleartext HTTP edge serves on loopback alone (RFC 7617 §4, RFC 6750 §5.3)"),
      EdgeRefused::StateGone => formatter.write_str("the HTTP edge's state is not on its shard"),
      EdgeRefused::Listener(error) => write!(formatter, "the HTTP edge's listener failed: {error}"),
    }
  }
}

/// A service failed: its request is refused with this status if no reply has begun, and its reply cut short if one has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Failed(pub(crate) Status);

/// A service's exchange for one request: it takes the request's body a run at a time and gives its reply a run at a time,
/// each run handed on before the next is asked (the engine writes before it reads again).
pub(crate) trait Exchange {
  /// Takes one run of the request's body, its framing and coding removed.
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed>;
  /// The request's body has ended.
  async fn body_end(&mut self) -> Result<(), Failed>;
  /// Appends the reply's next run to `out`: `true` while more follows, `false` once the reply has ended.
  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed>;
}

/// How a site answers a request's head.
pub(crate) enum Answer<E> {
  /// Serve it: `200` with `fields`, its body to `exchange` and its reply from it; the head alone when `head_only`.
  Serve {
    /// The reply's fields.
    fields: Fields,
    /// Whether the reply is the head alone (`HEAD`).
    head_only: bool,
    /// The service's exchange.
    exchange: E,
  },
  /// Refuse it with `status` and `fields`.
  Refuse {
    /// The status.
    status: Status,
    /// The fields.
    fields: Fields,
  },
}

/// A site's decision on a head: its answer, and whether the request's credentials proved a principal, which lets the
/// connection's waits teach the edge ([`waits`]).
pub(crate) struct Decided<E> {
  /// The answer.
  pub(crate) answer: Answer<E>,
  /// Whether the credentials proved a principal.
  pub(crate) proven: bool,
}

/// A tool's site: what decides each request's head (git's, [`git::GitSite`]).
pub(crate) trait Site {
  /// The exchange a served request runs.
  type Exchange: Exchange;
  /// Decides `head`, on a connection that is TLS when `secure`.
  async fn decide(&mut self, head: &Head<'_>, secure: bool) -> Decided<Self::Exchange>;
}

/// What an edge has counted: its non-vacuity counters, which its tests read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
  /// Connections accepted.
  pub(crate) accepted: u64,
  /// Times the listener waited for a seat, every one taken.
  pub(crate) full: u64,
  /// Connections ended because a wait ran past its learned bound.
  pub(crate) cut: u64,
  /// Connections ended because their socket failed, TLS failed, or no session could be made for them.
  pub(crate) failed: u64,
  /// Connections a refusal ended.
  pub(crate) refused: u64,
}

/// An edge's state on its shard, shared by its listener and its connections through a [`Kept`] handle: what bounds it,
/// the identity it presents, the seats taken and the listener waiting for one, every phase's learned waits, and its
/// counts. Nothing in it is lent across a wait.
pub(crate) struct EdgeState {
  /// The edge's bounds.
  bounds: Bounds,
  /// The identity HTTPS presents; none for a cleartext edge on loopback.
  identity: Option<ServingIdentity>,
  /// The seats taken: the connections being served.
  serving: Cell<usize>,
  /// The listener, when it waits for a seat.
  waiting: RefCell<Option<Waker>>,
  /// Every phase's learned waits.
  waits: RefCell<PhaseWaits>,
  /// The counts.
  counts: Cell<Counts>,
}

impl EdgeState {
  /// An edge's state: `bounds`, and the `identity` an HTTPS edge presents (none for a cleartext edge on loopback).
  pub(crate) fn new(bounds: Bounds, identity: Option<ServingIdentity>) -> EdgeState {
    EdgeState {
      bounds,
      identity,
      serving: Cell::new(0),
      waiting: RefCell::new(None),
      waits: RefCell::new(PhaseWaits::default()),
      counts: Cell::new(Counts::default()),
    }
  }

  /// What the edge has counted.
  pub(crate) fn counts(&self) -> Counts {
    self.counts.get()
  }

  /// The connections being served.
  pub(crate) fn serving(&self) -> usize {
    self.serving.get()
  }

  /// How many of `phase`'s waits completed and how many were cut; nothing while the waits are lent, which is never across
  /// a wait.
  pub(crate) fn learned(&self, phase: Phase) -> (u64, u64) {
    self
      .waits
      .try_borrow()
      .map_or((0, 0), |waits| (waits.samples(phase), waits.cuts(phase)))
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

  /// Whether a seat is free; when none is, the listener's `waker` is kept, to be woken when one is given back, and the
  /// wait counted once (`first`).
  fn seat_free(&self, waker: &Waker, first: bool) -> bool {
    if self.serving.get() < self.bounds.connections {
      return true;
    }
    if let Ok(mut waiting) = self.waiting.try_borrow_mut() {
      *waiting = Some(waker.clone());
    }
    if first {
      self.count(|counts| counts.full = counts.full.saturating_add(1));
    }
    false
  }

  /// Takes a seat for a connection just accepted.
  fn take_seat(&self) {
    self.serving.set(self.serving.get().saturating_add(1));
  }

  /// Gives a seat back, waking the listener if it waits for one.
  fn give_back(&self) {
    self.serving.set(self.serving.get().saturating_sub(1));
    let waiting = self
      .waiting
      .try_borrow_mut()
      .ok()
      .and_then(|mut waiting| waiting.take());
    if let Some(waker) = waiting {
      waker.wake();
    }
  }

  /// The engine for a connection just accepted: over a TLS session of the edge's identity, or cleartext; none when no
  /// session could be made.
  fn connection(&self) -> Option<Connection> {
    self.count(|counts| counts.accepted = counts.accepted.saturating_add(1));
    match &self.identity {
      Some(identity) => identity
        .connection()
        .ok()
        .map(|server| Connection::tls(HttpsSession::new(server), self.bounds.room)),
      None => Some(Connection::clear(self.bounds.room)),
    }
  }
}

/// A connection's seat among those its edge serves at once: taken as it is accepted and given back when this drops,
/// however the connection ends (its loop's end, its task's cancellation with its listener, a refused spawn), the
/// listener woken if it waits for one.
struct Seat {
  /// The edge.
  state: Kept<EdgeState>,
}

impl Seat {
  /// Takes a seat on the edge `state` keeps; none off its shard.
  fn take(state: Kept<EdgeState>) -> Option<Seat> {
    state.with(EdgeState::take_seat).map(|()| Seat { state })
  }
}

impl Drop for Seat {
  fn drop(&mut self) {
    // Off its shard (the runtime gone) no edge is left to give the seat back to.
    let _ = self.state.with(EdgeState::give_back);
  }
}

/// Waits until a seat is free (the module doc): at once below the bound, else until a connection ends.
async fn seat_free(state: Kept<EdgeState>) -> Result<(), EdgeRefused> {
  let mut first = true;
  std::future::poll_fn(|cx| {
    let free = state.with(|state| state.seat_free(cx.waker(), first));
    first = false;
    match free {
      Some(true) => Poll::Ready(Ok(())),
      Some(false) => Poll::Pending,
      None => Poll::Ready(Err(EdgeRefused::StateGone)),
    }
  })
  .await
}

/// Serves the edge `state` keeps on `listener` until the listener fails or its task ends (the module doc): each
/// connection a child task deciding its requests with a site `site` makes for it. Refused at once for a cleartext edge
/// off loopback, and when `state` is not on this shard.
pub(crate) async fn serve<S, M>(
  listener: TcpListener,
  state: Kept<EdgeState>,
  mut site: M,
) -> Result<(), EdgeRefused>
where
  S: Site + 'static,
  M: FnMut() -> S,
{
  let address = listener.local_addr().map_err(EdgeRefused::Listener)?;
  let secure = state
    .with(|state| state.identity.is_some())
    .ok_or(EdgeRefused::StateGone)?;
  if !secure && !admits_cleartext(address) {
    return Err(EdgeRefused::CleartextOffLoopback);
  }
  loop {
    seat_free(state).await?;
    let stream = listener.accept().await.map_err(EdgeRefused::Listener)?;
    let Some(connection) = state
      .with(EdgeState::connection)
      .ok_or(EdgeRefused::StateGone)?
    else {
      let _ =
        state.with(|state| state.count(|counts| counts.failed = counts.failed.saturating_add(1)));
      crate::fleet::count_refusal(NO_SESSION);
      continue;
    };
    let seat = Seat::take(state).ok_or(EdgeRefused::StateGone)?;
    match spawn_child(drive(stream, connection, site(), seat)) {
      Ok(task) => {
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

/// How a wait on a client ended.
enum Waited<T> {
  /// It completed after `waited_ns`.
  Done {
    /// What it brought.
    value: T,
    /// How long it waited.
    waited_ns: u64,
  },
  /// It ran past its learned bound.
  Cut,
  /// The socket failed, or no wait could be kept (off the shard).
  Failed,
}

/// Why a connection's loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
  /// The engine closed it.
  Closed,
  /// A wait ran past its learned bound.
  Cut,
  /// The socket failed.
  Failed,
}

/// Whether a connection's loop goes on.
enum Flow {
  /// It does.
  Go,
  /// It ends.
  End(Why),
}

/// A connection's socket and its read buffer.
struct Socket {
  /// The stream.
  stream: TcpStream,
  /// What one read takes.
  buffer: Vec<u8>,
}

impl Socket {
  /// Reads within `deadline_ns` (none: until the client's bytes or its close): the count read, zero at its close.
  async fn read(&mut self, deadline_ns: Option<u64>) -> Waited<usize> {
    let started_ns = now_ns();
    let read = self.stream.read(&mut self.buffer);
    let outcome = match deadline_ns {
      None => Ok(Some(read.await)),
      Some(deadline_ns) => within(deadline_ns.saturating_sub(started_ns), read).await,
    };
    waited(outcome, started_ns)
  }

  /// Writes `bytes` whole within `deadline_ns` (none: until the client takes them or the socket fails).
  async fn write(&mut self, bytes: &[u8], deadline_ns: Option<u64>) -> Waited<()> {
    let started_ns = now_ns();
    let write = self.stream.write_all(bytes);
    let outcome = match deadline_ns {
      None => Ok(Some(write.await)),
      Some(deadline_ns) => within(deadline_ns.saturating_sub(started_ns), write).await,
    };
    waited(outcome, started_ns)
  }

  /// The bytes the last read brought, `count` of them.
  fn read_bytes(&self, count: usize) -> &[u8] {
    self.buffer.get(..count).unwrap_or_default()
  }
}

/// A wait raced against its deadline (`slates_rt::futures::within`): the work's own outcome when it finished first, none
/// once the deadline passed, or the race's refusal off a shard.
type Raced<T> = Result<Option<Result<T, RtError>>, RtError>;

/// How a wait begun at `started_ns` ended, from its raced outcome.
fn waited<T>(outcome: Raced<T>, started_ns: u64) -> Waited<T> {
  match outcome {
    Ok(Some(Ok(value))) => Waited::Done {
      value,
      waited_ns: now_ns().saturating_sub(started_ns),
    },
    Ok(None) => Waited::Cut,
    Ok(Some(Err(_))) | Err(_) => Waited::Failed,
  }
}

/// A service a decided request started, and whether its reply is the head alone.
struct Started<E> {
  /// The exchange.
  exchange: E,
  /// Whether the reply is the head alone.
  head_only: bool,
}

/// One connection's parts beside its engine: its socket, its edge, its waits, its site and the service its request
/// started.
struct Parts<S: Site> {
  /// The socket.
  socket: Socket,
  /// The edge.
  state: Kept<EdgeState>,
  /// The connection's waits.
  tracker: Tracker,
  /// The site deciding its requests.
  site: S,
  /// Whether the connection is TLS.
  secure: bool,
  /// The service the request being served started.
  service: Option<Started<S::Exchange>>,
  /// The reply's run being handed on.
  reply: Vec<u8>,
}

impl<S: Site> Parts<S> {
  /// Runs `f` over the connection's waits and the edge's learned ones; none off the edge's shard.
  fn learning<R>(&mut self, f: impl FnOnce(&mut Tracker, &mut PhaseWaits) -> R) -> Option<R> {
    let tracker = &mut self.tracker;
    self
      .state
      .with(|state| state.with_waits(|waits| f(tracker, waits)))
      .flatten()
  }

  /// Where a wait in `phase` starting now ends ([`Tracker::deadline_ns`]): `Err` off the edge's shard, where no wait can
  /// be kept.
  fn deadline_ns(&mut self, phase: Phase) -> Result<Option<u64>, ()> {
    let now = now_ns();
    self
      .learning(|tracker, waits| tracker.deadline_ns(phase, now, waits))
      .ok_or(())
  }

  /// How a wait in `phase` ended: a completed one heard, a cut one cut, a failed one abandoned; and the loop's flow after.
  fn settle<T>(&mut self, phase: Phase, waited: Waited<T>) -> Result<T, Why> {
    match waited {
      Waited::Done { value, waited_ns } => {
        let _ = self.learning(|tracker, waits| tracker.heard(phase, waited_ns, waits));
        Ok(value)
      }
      Waited::Cut => {
        let _ = self.learning(|tracker, waits| tracker.cut(phase, waits));
        Err(Why::Cut)
      }
      Waited::Failed => {
        self.tracker.abandon();
        Err(Why::Failed)
      }
    }
  }

  /// Writes what the engine has to send, within the phase's wait.
  async fn write(&mut self, connection: &mut Connection) -> Flow {
    let phase = connection.phase();
    let bytes = connection.take_output();
    let Ok(deadline_ns) = self.deadline_ns(phase) else {
      return Flow::End(Why::Failed);
    };
    let waited = self.socket.write(&bytes, deadline_ns).await;
    match self.settle(phase, waited) {
      Ok(()) => Flow::Go,
      Err(why) => Flow::End(why),
    }
  }

  /// Reads within the phase's wait and hands the bytes, or the client's close, to the engine.
  async fn read(&mut self, connection: &mut Connection) -> Flow {
    let phase = connection.phase();
    let Ok(deadline_ns) = self.deadline_ns(phase) else {
      return Flow::End(Why::Failed);
    };
    let count = match self.socket.read(deadline_ns).await {
      Waited::Done { value: 0, .. } => {
        // The client left: whatever it was waited for, the wait's true length is unknown, so it teaches nothing.
        self.tracker.abandon();
        connection.peer_closed();
        return Flow::Go;
      }
      waited => match self.settle(phase, waited) {
        Ok(count) => count,
        Err(why) => return Flow::End(why),
      },
    };
    connection.receive(self.socket.read_bytes(count));
    // The bytes may have ended a whole phase: the first byte of a request ends its idleness and begins its head.
    let (after, now) = (connection.phase(), now_ns());
    let _ = self.learning(|tracker, waits| tracker.observe(after, now, waits));
    Flow::Go
  }

  /// Asks the site to decide the whole head, and gives the engine its decision.
  async fn decide(&mut self, connection: &mut Connection) -> Flow {
    let now = now_ns();
    // The head is whole: its wait completed.
    let _ = self.learning(|tracker, waits| tracker.end(now, waits));
    let decided = match connection.head() {
      Some(head) => self.site.decide(&head, self.secure).await,
      None => Decided {
        answer: Answer::Refuse {
          status: Status::INTERNAL_SERVER_ERROR,
          fields: Fields::new(),
        },
        proven: false,
      },
    };
    if decided.proven {
      let _ = self.learning(|tracker, waits| tracker.prove(waits));
    }
    match decided.answer {
      Answer::Serve {
        fields,
        head_only,
        exchange,
      } => {
        self.service = Some(Started {
          exchange,
          head_only,
        });
        connection.decide(Decision::Serve { fields, head_only });
      }
      Answer::Refuse { status, fields } => {
        self.service = None;
        connection.decide(Decision::Refuse { status, fields });
      }
    }
    Flow::Go
  }

  /// Hands one run of the request's body to the service.
  async fn take_body(&mut self, run: &[u8]) -> Result<(), Failed> {
    match self.service.as_mut() {
      Some(started) => started.exchange.body(run).await,
      None => Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    }
  }

  /// Tells the engine whether the service took the body's run, then yields: one run a turn (the module doc).
  async fn body_taken(&mut self, connection: &mut Connection, taken: Result<(), Failed>) -> Flow {
    match taken {
      Ok(()) => connection.body_taken(),
      Err(Failed(status)) => {
        self.service = None;
        connection.fail(status);
      }
    }
    yield_now().await;
    Flow::Go
  }

  /// Tells the service the request's body ended.
  async fn body_end(&mut self, connection: &mut Connection) -> Flow {
    let ended = match self.service.as_mut() {
      Some(started) => started.exchange.body_end().await,
      None => Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    };
    if let Err(Failed(status)) = ended {
      self.service = None;
      connection.fail(status);
    }
    Flow::Go
  }

  /// Passes the service's next reply run to the engine, or the reply's end, then yields: one run a turn.
  async fn reply(&mut self, connection: &mut Connection) -> Flow {
    let Some(started) = self.service.as_mut() else {
      connection.fail(Status::INTERNAL_SERVER_ERROR);
      return Flow::Go;
    };
    if started.head_only {
      self.service = None;
      connection.reply_end();
      return Flow::Go;
    }
    self.reply.clear();
    match started.exchange.reply(&mut self.reply).await {
      Ok(true) => connection.reply(&self.reply),
      Ok(false) => {
        if !self.reply.is_empty() {
          connection.reply(&self.reply);
        }
        self.service = None;
        connection.reply_end();
      }
      Err(Failed(status)) => {
        self.service = None;
        connection.fail(status);
      }
    }
    yield_now().await;
    Flow::Go
  }

  /// Closes the socket's write side for a staged close, which only a refusal begins: the refusal is counted here, as it
  /// ends the connection, before the client can read the end of it.
  fn close_write(&mut self) -> Flow {
    let _ = self.state.with(|state| {
      state.count(|counts| counts.refused = counts.refused.saturating_add(1));
    });
    match self.socket.stream.close_write() {
      Ok(()) => Flow::Go,
      Err(_) => Flow::End(Why::Failed),
    }
  }

  /// Reads and drops the client's bytes until it closes, within the drain's wait, a read a turn.
  async fn drain(&mut self) -> Why {
    loop {
      let Ok(deadline_ns) = self.deadline_ns(Phase::Drain) else {
        return Why::Failed;
      };
      let waited = self.socket.read(deadline_ns).await;
      match self.settle(Phase::Drain, waited) {
        Ok(0) => {
          let now = now_ns();
          // The client closed: the drain's wait completed.
          let _ = self.learning(|tracker, waits| tracker.end(now, waits));
          return Why::Closed;
        }
        Ok(_) => yield_now().await,
        Err(why) => return why,
      }
    }
  }
}

/// Serves one connection until it ends (the module doc's loop), its seat given back however it ends.
async fn drive<S: Site>(stream: TcpStream, mut connection: Connection, site: S, seat: Seat) {
  let mut parts = Parts {
    socket: Socket {
      stream,
      buffer: vec![0; READ_BYTES],
    },
    state: seat.state,
    tracker: Tracker::default(),
    site,
    secure: connection.secure(),
    service: None,
    reply: Vec::new(),
  };
  let why = loop {
    let flow = match connection.next() {
      Next::Write => parts.write(&mut connection).await,
      Next::Read => parts.read(&mut connection).await,
      Next::Decide => parts.decide(&mut connection).await,
      Next::Body(run) => {
        let taken = parts.take_body(run).await;
        parts.body_taken(&mut connection, taken).await
      }
      Next::BodyEnd => parts.body_end(&mut connection).await,
      Next::Reply => parts.reply(&mut connection).await,
      Next::CloseWrite => parts.close_write(),
      Next::Drain => Flow::End(parts.drain().await),
      Next::Close => Flow::End(Why::Closed),
    };
    if let Flow::End(why) = flow {
      break why;
    }
  };
  count_end(parts.state, why, connection.ended());
  drop(seat);
}

/// Counts how a connection ended: by its loop's reason, then by the engine's. Called before the connection's socket
/// closes, so its client cannot see the end before the count.
fn count_end(state: Kept<EdgeState>, why: Why, ended: Option<Ended>) {
  match why {
    Why::Cut => {
      crate::fleet::count_refusal(CUT);
    }
    Why::Failed => {
      crate::fleet::count_refusal(SOCKET_FAILED);
    }
    Why::Closed => {}
  }
  // A refusal was counted as it ended the connection (`Parts::close_write`).
  let _ = state.with(|state| {
    state.count(|counts| match (why, ended) {
      (Why::Cut, _) => counts.cut = counts.cut.saturating_add(1),
      (Why::Failed, _) | (Why::Closed, Some(Ended::Tls | Ended::Aborted)) => {
        counts.failed = counts.failed.saturating_add(1);
      }
      (Why::Closed, _) => {}
    });
  });
}

#[cfg(test)]
#[path = "vcs_http/tests/serve.rs"]
mod tests;
