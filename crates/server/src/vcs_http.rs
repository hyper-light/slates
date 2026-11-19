//! The version-control servers' HTTP edge on the daemon's runtime (A-625 part 2; §4.17.2 "Transports" item 1 and
//! "Pieces (git)" piece 8; §4.3, R6): a listener on the runtime's TCP, one connection a task, each driving
//! `slates-http-edge`'s engine (A-625 part 1) over its socket; every wait on a client learned per phase ([`waits`]); the
//! connections served at once and each request's room derived from the shard's budget ([`Bounds`]); and a site that
//! decides each request ([`Site`]): git's ([`git`]) first, Mercurial's ([`hg`], A-650), and Subversion's ([`svn`],
//! A-681: its read side's resources, OPTIONS, PROPFIND, GET and HEAD, answered by `slates_svn::dav::site`) on the same
//! edge, the daemon's handing each request to its tool's ([`ToolSites`], A-680 part 2a: Mercurial's for a query naming a
//! command, as hgweb tells a wire request, git's for every other). A reply's status is `200` unless the exchange names
//! another as its reply begins ([`Exchange::reply_status`]: a PROPFIND's `207`, or a refusal its body explains).
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
//! **A wait past its bound marks its connection overdue; the most overdue is reaped when a seat is needed** (main's
//! ruling, 2026-11-19, the rule g294 found for SSH). A connection's every read and write runs against its phase's bound
//! ([`waits`]); one still waiting at its deadline goes on waiting and is marked overdue in its seat, since a git client
//! may be silent for long mid-request (compressing a large push, a credential helper prompting) and a cut would fail it.
//! The listener accepts the next connection whenever it comes; when every seat is taken it reaps the most overdue
//! connection for it (its task cancelled, its seat given back as its guard drops; the reap teaches nothing and backs the
//! phase off, [`waits::PhaseWaits::reap`]), and with none overdue it waits until a seat is given back or a connection
//! becomes overdue. So the listener holds at most one accepted connection beyond the seats, the kernel's queue behind it
//! is the same bound ([`Bounds::backlog`]), a silent client never holds a seat a new connection needs, and none is cut
//! while seats are free. No connection is answered `503` at the bound (main's ruling): over HTTPS only a handshake could
//! carry it, which costs the task and buffers the bound limits, and a refusal closed with the client's bytes unread is a
//! reset that can erase it (A-183). The listener's waits for a seat, the overdue marks and the reaps are counted
//! ([`Counts`]).
//!
//! **A connection moves to its repository's shard** (main's ruling, 2026-11-19; A-625 part 3). Once a proven request's
//! head names a repository another shard's partition holds, the connection goes there whole, as the NFS export moves a
//! mount's connection (§4.6, `crate::nfs`'s `migrate`): its socket's handle, its engine with the head still undecided,
//! and its waits, as one spawn on that shard's control channel, sharing by move. Its seat here is given back first, and
//! there it takes one of that shard's seats as a new connection would ([`seat_arrival`]): a free one, or the most overdue
//! connection's, or it waits, and at most as many arrivals wait as the shard has seats, past which the arrival is closed
//! and counted, as a refused move closes an NFS connection. There the head is decided again, and every step lends the
//! repository's store on its own shard with no crossing. Each shard keeps its own edge state ([`keep_here`]): its seats,
//! its learned waits and its counts.
//!
//! **One connection's loop** asks the engine what it needs next and does it: writes its bytes, reads within the phase's
//! wait, asks the site to decide a whole head (the answer says whether the credentials proved a principal), hands each run
//! of the body to the service the site started and then its end, passes the service's reply on, and half-closes and
//! drains for a staged close. One body run or one reply run a turn (§4.3): the task yields after each, so a large push or
//! clone never holds its shard, and a drain yields after each read it drops.
//!
//! **Built since:** the daemon starting an edge (A-625 part 3: its address, the operator's identity, an authenticator
//! over §4.13's principals and keys, the repositories' lookup on their partitions; `--vcs-http` from the CLI); git's
//! receive-pack through the push driver on the repository's owner shard (A-654 part 3); Mercurial's site on the daemon's
//! edge, each repository's served view lent from its store on its owner shard (A-680 part 2a).
//!
//! **Owed** (GAPS, the §4.17 entry): upload-pack's server half
//! handed the stores (A-615, A-662), which answers `503` until then;
//! Mercurial's pushes (`unbundle`) through the push driver (A-680 part 2b);
//! Subversion's site handed its stores (each repository's index, green, lock table and authorization file, and a
//! principal's Subversion user) and its REPORTs, commits and locks (A-681's later parts), which it answers `503` and
//! `501` until then; Subversion's site composed into the daemon's beside git's and Mercurial's ([`ToolSites`]); the MCP
//! edge onto this engine; the live `git clone`, `git push` and `hg clone` over HTTPS (main's runs).

mod daemon_edge;
pub(crate) mod git;
pub(crate) mod hg;
mod svn;
pub(crate) mod waits;

pub(crate) use daemon_edge::{ShardRepositories, bind, bounds_of, start};

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::task::{Poll, Waker};

use slates_http::head::Head;
use slates_http::reply::Status;
use slates_http_edge::connection::{Connection, Decision, Ended, Fields, Next, Phase};
use slates_http_edge::tls::{HttpsSession, ServingIdentity};
use slates_rt::TaskId;
use slates_rt::error::RtError;
use slates_rt::futures::{
  cancel, detach, now_ns, scheduler_overrun_ns, sleep, spawn_child, yield_now,
};
use slates_rt::shard::Kept;
use slates_rt::tcp::{OwnedStream, SocketAddrV4, TcpListener, TcpStream};

use waits::{Overdue, PhaseWaits, Tracker};

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
/// A connection whose round trip its kernel could not report: with no measurement of its own its first waits could not
/// be bounded, so it is not served.
const NO_ROUND_TRIP: &str = "vcs_http.no_round_trip";
/// An overdue connection reaped for a new one.
const REAPED: &str = "vcs_http.reaped";
/// A reap whose task could not be cancelled (it had ended: its seat comes back as it is reaped).
const NOT_REAPED: &str = "vcs_http.not_reaped";
/// A connection whose socket failed.
const SOCKET_FAILED: &str = "vcs_http.socket";
/// A move its target shard's control channel refused: the descriptor dropped with the spawn, so the connection closed.
const MOVE_REFUSED: &str = "vcs_http.move_refused";
/// A connection that arrived on a shard keeping no edge state, or whose socket could not be adopted there.
const ARRIVAL_LOST: &str = "vcs_http.arrival_lost";
/// A connection that arrived on a shard whose seats were all taken with as many arrivals already waiting as it has
/// seats: closed.
const ARRIVAL_REFUSED: &str = "vcs_http.arrival_refused";

/// What bounds an edge, derived from its shard's budget (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
  /// The connections served at once.
  pub(crate) connections: usize,
  /// The bytes each request may hold.
  pub(crate) room: u64,
  /// The shard's timer tick (`RuntimeConfig::timer_tick_ns`, the profile's `max(wake.mean, 100 × timer overhead)`): the
  /// finest wait its timers keep, so the least clock granularity `G` its waits are floored by ([`waits`]).
  pub(crate) tick_ns: u64,
}

impl Bounds {
  /// Derived: an edge on a shard that seats `clients_per_shard` clients, each over a region of `region_bytes`, whose timer
  /// ticks every `tick_ns`: one connection a seat, and each request one region's room (the module doc).
  pub(crate) fn of_shard(clients_per_shard: usize, region_bytes: usize, tick_ns: u64) -> Bounds {
    Bounds {
      connections: clients_per_shard.max(1),
      room: u64::try_from(region_bytes).unwrap_or(u64::MAX),
      tick_ns,
    }
  }

  /// Derived: the listen backlog, the connections the kernel queues for the listener: as many as the edge serves at once
  /// (the module doc), clamped to what the call takes; the OS clamps it further to its own maximum.
  pub(crate) fn backlog(self) -> i32 {
    i32::try_from(self.connections).unwrap_or(i32::MAX)
  }

  /// The clock granularity `G` a wait starting now is floored by: the timer's tick, or the lateness the shard's scheduler
  /// has shown when that is longer (`slates_rt::futures::scheduler_overrun_ns`, the floor the fleet's detector and the MCP
  /// edge put under their windows).
  pub(crate) fn granularity_ns(self) -> u64 {
    self.tick_ns.max(scheduler_overrun_ns())
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
  /// The fields the reply's head carries after the decision's, asked once as the reply begins, after the body has
  /// ended: none unless the reply's outcome decides them, as a Mercurial command's decides its media type (A-650).
  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    Ok(Fields::new())
  }
  /// The reply's status, asked once as the reply begins, after the body has ended and before the reply's fields: `200`
  /// unless the reply's outcome decides another, as a WebDAV `PROPFIND`'s `207` or a refusal its body explains
  /// (Subversion's site, A-681).
  fn reply_status(&self) -> Status {
    Status::OK
  }
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
  /// Decide it on runtime shard `shard`, where its repository lives: the connection moves there whole, its head
  /// undecided (the module doc).
  Move {
    /// The shard.
    shard: u16,
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

impl<E> Decided<E> {
  /// The same decision, its exchange wrapped by `wrap` where it serves.
  pub(crate) fn map<F>(self, wrap: impl FnOnce(E) -> F) -> Decided<F> {
    let answer = match self.answer {
      Answer::Serve {
        fields,
        head_only,
        exchange,
      } => Answer::Serve {
        fields,
        head_only,
        exchange: wrap(exchange),
      },
      Answer::Refuse { status, fields } => Answer::Refuse { status, fields },
      Answer::Move { shard } => Answer::Move { shard },
    };
    Decided {
      answer,
      proven: self.proven,
    }
  }
}

/// The daemon's sites on one edge (A-680 part 2): Mercurial's decides a wire request, whose query names a command
/// ([`hg::wire_request`]), and git's every other request, so each tool's clients reach their repositories at one address.
/// A repository of the other tool is answered as one that does not exist, by the site that was asked.
pub(crate) struct ToolSites<G, H> {
  /// git's site.
  git: G,
  /// Mercurial's site.
  hg: H,
  /// The room a request's query is read within to tell a Mercurial request: the edge's request room.
  room: u64,
}

impl<G, H> ToolSites<G, H> {
  /// git's site `git` and Mercurial's `hg` on one edge, a query read within `room`.
  pub(crate) fn new(git: G, hg: H, room: u64) -> Self {
    Self { git, hg, room }
  }
}

/// The exchange of a request one of [`ToolSites`]' sites serves.
pub(crate) enum ToolExchange<G, H> {
  /// git's.
  Git(G),
  /// Mercurial's.
  Hg(H),
}

impl<G: Exchange, H: Exchange> Exchange for ToolExchange<G, H> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    match self {
      Self::Git(exchange) => exchange.body(run).await,
      Self::Hg(exchange) => exchange.body(run).await,
    }
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    match self {
      Self::Git(exchange) => exchange.body_end().await,
      Self::Hg(exchange) => exchange.body_end().await,
    }
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    match self {
      Self::Git(exchange) => exchange.reply_fields().await,
      Self::Hg(exchange) => exchange.reply_fields().await,
    }
  }

  fn reply_status(&self) -> Status {
    match self {
      Self::Git(exchange) => exchange.reply_status(),
      Self::Hg(exchange) => exchange.reply_status(),
    }
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    match self {
      Self::Git(exchange) => exchange.reply(out).await,
      Self::Hg(exchange) => exchange.reply(out).await,
    }
  }
}

impl<G: Site, H: Site> Site for ToolSites<G, H> {
  type Exchange = ToolExchange<G::Exchange, H::Exchange>;

  async fn decide(&mut self, head: &Head<'_>, secure: bool) -> Decided<Self::Exchange> {
    if hg::wire_request(head, self.room) {
      self.hg.decide(head, secure).await.map(ToolExchange::Hg)
    } else {
      self.git.decide(head, secure).await.map(ToolExchange::Git)
    }
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
  /// Connections ended because their socket failed, TLS failed, a reply was cut short, or no session or round trip could
  /// be had for them.
  pub(crate) failed: u64,
  /// Connections a refusal ended.
  pub(crate) refused: u64,
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
/// the identity it presents, its seats, the listener waiting for one, every phase's learned waits, and its counts.
/// Nothing in it is lent across a wait.
pub(crate) struct EdgeState {
  /// The edge's bounds.
  bounds: Bounds,
  /// The identity HTTPS presents; none for a cleartext edge on loopback.
  identity: Option<ServingIdentity>,
  /// Each seat, free or taken: as many as the edge serves at once.
  seats: RefCell<Vec<Option<Taken>>>,
  /// Who waits for a seat: the listener, and connections that moved here, at most one of each kind a seat and the
  /// listener ([`EdgeState::arriving`]).
  waiting: RefCell<Vec<Waker>>,
  /// The connections that moved here and wait for a seat: at most as many as the shard has seats.
  arriving: Cell<usize>,
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
      seats: RefCell::new(vec![None; bounds.connections]),
      waiting: RefCell::new(Vec::new()),
      arriving: Cell::new(0),
      waits: RefCell::new(PhaseWaits::default()),
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

  /// How the listener, holding an accepted connection, stands for a seat (the module doc): a free seat; or, with every
  /// seat taken, the most overdue connection not already being reaped, chosen now (its reap counted, teaching nothing and
  /// backing its phase off); or a wait, the listener's `waker` kept to be woken when a seat comes back or a connection
  /// becomes overdue. A first look with every seat taken (`first`) is counted.
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

  /// Marks seat `slot`'s connection overdue as `overdue` says, or clears its mark (`None`); a mark wakes the listener,
  /// which may hold a connection waiting for a seat.
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

  /// Gives seat `slot` back, waking the listener if it waits for one.
  fn give_back(&self, slot: usize) {
    let _ = self.with_seats(|seats| {
      if let Some(seat) = seats.get_mut(slot) {
        *seat = None;
      }
    });
    self.wake_listener();
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
/// however the connection ends (its loop's end, its task's cancellation by a reap or with its listener, a refused spawn),
/// the listener woken if it waits for one.
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

thread_local! {
  /// This shard's edge state, once kept here ([`keep_here`]): where a connection that moves here is seated.
  static HERE: Cell<Option<Kept<EdgeState>>> = const { Cell::new(None) };
}

/// Keeps `state` on this shard for its life as the shard's edge (the module doc): the listener's shard keeps the one
/// with the identity, every other shard one without, so a connection that moves to a shard finds its seats there. Called
/// once a shard, at the daemon's start; refused off a shard's thread.
pub(crate) fn keep_here(state: EdgeState) -> Result<Kept<EdgeState>, RtError> {
  let kept = slates_rt::registry::with_current(|context| context.keep(state))
    .ok_or(RtError::NotOnShardThread)??;
  HERE.with(|here| here.set(Some(kept)));
  Ok(kept)
}

/// This shard's edge state, if one was kept here.
fn here() -> Option<Kept<EdgeState>> {
  HERE.with(Cell::get)
}

/// Serves the edge `state` keeps on `listener` until the listener fails or its task ends (the module doc): each
/// connection a child task deciding its requests with a site `site` makes for it, here or on the shard it moves to.
/// Refused at once for a cleartext edge off loopback, and when `state` is not on this shard.
pub(crate) async fn serve<S, M>(
  listener: TcpListener,
  state: Kept<EdgeState>,
  site: M,
) -> Result<(), EdgeRefused>
where
  S: Site + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let address = listener.local_addr().map_err(EdgeRefused::Listener)?;
  let secure = state
    .with(|state| state.identity.is_some())
    .ok_or(EdgeRefused::StateGone)?;
  if !secure && !admits_cleartext(address) {
    return Err(EdgeRefused::CleartextOffLoopback);
  }
  loop {
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
    let Ok(round_trip) = stream.round_trip() else {
      let _ =
        state.with(|state| state.count(|counts| counts.failed = counts.failed.saturating_add(1)));
      crate::fleet::count_refusal(NO_ROUND_TRIP);
      continue;
    };
    let slot = seat_for(state).await?;
    let seat = Seat::take(state, slot).ok_or(EdgeRefused::StateGone)?;
    let tracker = Tracker::new(round_trip);
    match spawn_child(drive(stream, connection, site.clone(), seat, tracker)) {
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

/// How a wait on a client ended: it never ends at its bound, which only marks its connection overdue.
enum Waited<T> {
  /// It completed after `waited_ns`.
  Done {
    /// What it brought.
    value: T,
    /// How long it waited.
    waited_ns: u64,
  },
  /// The socket failed, or no deadline could be armed (off the shard).
  Failed,
}

/// Why a connection's loop ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Why {
  /// The engine closed it.
  Closed,
  /// The socket failed.
  Failed,
}

/// Whether a connection's loop goes on.
enum Flow {
  /// It does.
  Go,
  /// It ends.
  End(Why),
  /// It goes on on runtime shard `shard`, its head undecided.
  Move(u16),
}

/// Awaits `work` with no end of its own: once `deadline_ns` (on the shard's clock) passes with it still waiting,
/// `overdue` is called, once, and the same work goes on until it ends or its task is reaped (never dropped mid-way, so a
/// write past its deadline loses no byte it handed the kernel). The work's output and whether it was overdue; refused
/// only where no deadline can be armed (off a shard).
pub(crate) async fn awaited<F: Future>(
  work: F,
  deadline_ns: u64,
  mut overdue: impl FnMut(),
) -> Result<(F::Output, bool), RtError> {
  let mut work = std::pin::pin!(work);
  let mut deadline = std::pin::pin!(sleep(deadline_ns.saturating_sub(now_ns())));
  let mut passed = false;
  std::future::poll_fn(|cx| {
    if let Poll::Ready(output) = work.as_mut().poll(cx) {
      return Poll::Ready(Ok((output, passed)));
    }
    if !passed {
      match deadline.as_mut().poll(cx) {
        Poll::Ready(Ok(())) => {
          passed = true;
          overdue();
        }
        Poll::Ready(Err(refusal)) => return Poll::Ready(Err(refusal)),
        Poll::Pending => {}
      }
    }
    Poll::Pending
  })
  .await
}

/// A connection's socket and its read buffer.
struct Socket {
  /// The stream.
  stream: TcpStream,
  /// What one read takes.
  buffer: Vec<u8>,
}

impl Socket {
  /// Reads, `overdue` called if `deadline_ns` passes first: the count read (zero at the client's close), and whether the
  /// wait was overdue.
  async fn read(&mut self, deadline_ns: u64, overdue: impl FnMut()) -> (Waited<usize>, bool) {
    let started_ns = now_ns();
    let read = self.stream.read(&mut self.buffer);
    waited(awaited(read, deadline_ns, overdue).await, started_ns)
  }

  /// Writes `bytes` whole, `overdue` called if `deadline_ns` passes first; and whether the wait was overdue.
  async fn write(
    &mut self,
    bytes: &[u8],
    deadline_ns: u64,
    overdue: impl FnMut(),
  ) -> (Waited<()>, bool) {
    let started_ns = now_ns();
    let write = self.stream.write_all(bytes);
    waited(awaited(write, deadline_ns, overdue).await, started_ns)
  }

  /// The bytes the last read brought, `count` of them.
  fn read_bytes(&self, count: usize) -> &[u8] {
    self.buffer.get(..count).unwrap_or_default()
  }
}

/// A wait's outcome: the work's own when it ended (an error is a failure), and whether it was overdue.
type Raced<T> = Result<(Result<T, RtError>, bool), RtError>;

/// How a wait begun at `started_ns` ended, from its outcome, and whether it was overdue.
fn waited<T>(outcome: Raced<T>, started_ns: u64) -> (Waited<T>, bool) {
  match outcome {
    Ok((Ok(value), overdue)) => (
      Waited::Done {
        value,
        waited_ns: now_ns().saturating_sub(started_ns),
      },
      overdue,
    ),
    Ok((Err(_), overdue)) => (Waited::Failed, overdue),
    Err(_) => (Waited::Failed, false),
  }
}

/// A service a decided request started, whether its reply is the head alone, and whether its reply's fields were asked.
struct Started<E> {
  /// The exchange.
  exchange: E,
  /// Whether the reply is the head alone.
  head_only: bool,
  /// Whether the exchange's reply fields have been asked for and given to the engine.
  fields_given: bool,
}

/// One connection's parts beside its engine: its socket, its edge and seat, its waits, its site and the service its
/// request started.
struct Parts<S: Site> {
  /// The socket.
  socket: Socket,
  /// The edge.
  state: Kept<EdgeState>,
  /// Its seat.
  slot: usize,
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

  /// Where a wait in `phase` starting now passes its bound ([`Tracker::deadline_ns`]), and the mark its connection takes
  /// then: `Err` off the edge's shard, where no wait can be kept.
  fn armed(&mut self, phase: Phase) -> Result<(u64, Overdue), ()> {
    let now = now_ns();
    let granularity_ns = self
      .state
      .with(|state| state.bounds.granularity_ns())
      .ok_or(())?;
    let deadline_ns = self
      .learning(|tracker, waits| tracker.deadline_ns(phase, now, granularity_ns, waits))
      .ok_or(())?;
    Ok((deadline_ns, self.tracker.overdue(phase, deadline_ns)))
  }

  /// What marks the connection's seat overdue as `overdue` says, when its wait passes its bound.
  fn marker(&self, overdue: Overdue) -> impl FnMut() + use<S> {
    let (state, slot) = (self.state, self.slot);
    move || {
      let _ = state.with(|state| state.mark(slot, Some(overdue)));
    }
  }

  /// A wait ended: its overdue mark, if it took one, cleared; and, completed, it is heard in `phase` (however late).
  fn settle<T>(&mut self, phase: Phase, (waited, overdue): (Waited<T>, bool)) -> Result<T, Why> {
    if overdue {
      let slot = self.slot;
      let _ = self.state.with(|state| state.mark(slot, None));
    }
    match waited {
      Waited::Done { value, waited_ns } => {
        let _ = self.learning(|tracker, waits| tracker.heard(phase, waited_ns, waits));
        Ok(value)
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
    let Ok((deadline_ns, overdue)) = self.armed(phase) else {
      return Flow::End(Why::Failed);
    };
    let marker = self.marker(overdue);
    let waited = self.socket.write(&bytes, deadline_ns, marker).await;
    match self.settle(phase, waited) {
      Ok(()) => Flow::Go,
      Err(why) => Flow::End(why),
    }
  }

  /// Reads within the phase's wait and hands the bytes, or the client's close, to the engine.
  async fn read(&mut self, connection: &mut Connection) -> Flow {
    let phase = connection.phase();
    let Ok((deadline_ns, overdue)) = self.armed(phase) else {
      return Flow::End(Why::Failed);
    };
    let marker = self.marker(overdue);
    let count = match self.socket.read(deadline_ns, marker).await {
      (Waited::Done { value: 0, .. }, overdue) => {
        if overdue {
          let slot = self.slot;
          let _ = self.state.with(|state| state.mark(slot, None));
        }
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
    let receiving_ns = now_ns();
    connection.receive(self.socket.read_bytes(count));
    let now = now_ns();
    if phase == Phase::Handshake {
      // The edge's own work on the handshake (its key exchange, its signature, its records), which the client's share
      // of the handshake matches: added to the connection's first measurement while the handshake runs ([`waits`]).
      self.tracker.worked(now.saturating_sub(receiving_ns));
    }
    // The bytes may have ended a whole phase: the first byte of a request ends its idleness and begins its head.
    let after = connection.phase();
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
      Answer::Move { shard } => return Flow::Move(shard),
      Answer::Serve {
        fields,
        head_only,
        exchange,
      } => {
        self.service = Some(Started {
          exchange,
          head_only,
          fields_given: false,
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

  /// Passes the service's next reply run to the engine, or the reply's end, then yields: one run a turn. The service's
  /// reply fields go to the engine first, once, before any of the reply.
  async fn reply(&mut self, connection: &mut Connection) -> Flow {
    let Some(started) = self.service.as_mut() else {
      connection.fail(Status::INTERNAL_SERVER_ERROR);
      return Flow::Go;
    };
    if !started.fields_given {
      started.fields_given = true;
      connection.reply_status(started.exchange.reply_status());
      match started.exchange.reply_fields().await {
        Ok(fields) => connection.reply_fields(fields),
        Err(Failed(status)) => {
          self.service = None;
          connection.fail(status);
          return Flow::Go;
        }
      }
    }
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
      let Ok((deadline_ns, overdue)) = self.armed(Phase::Drain) else {
        return Why::Failed;
      };
      let marker = self.marker(overdue);
      let waited = self.socket.read(deadline_ns, marker).await;
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

/// How a connection's loop left: it ended, or its connection moves to another shard.
enum Exit {
  /// It ended.
  Ended(Why),
  /// It moves to runtime shard `shard`.
  Moved(u16),
}

/// Serves one connection until it ends or moves to its repository's shard (the module doc's loop), its seat here given
/// back however it leaves; `make` makes its site here, and there.
async fn drive<S, M>(
  stream: TcpStream,
  mut connection: Connection,
  make: M,
  seat: Seat,
  tracker: Tracker,
) where
  S: Site + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let mut parts = Parts {
    socket: Socket {
      stream,
      buffer: vec![0; READ_BYTES],
    },
    state: seat.state,
    slot: seat.slot,
    tracker,
    site: make(),
    secure: connection.secure(),
    service: None,
    reply: Vec::new(),
  };
  let exit = loop {
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
    match flow {
      Flow::Go => {}
      Flow::End(why) => break Exit::Ended(why),
      Flow::Move(shard) => break Exit::Moved(shard),
    }
  };
  match exit {
    Exit::Ended(why) => {
      count_end(parts.state, why, connection.ended());
      drop(seat);
    }
    Exit::Moved(shard) => {
      let Parts {
        socket,
        tracker,
        state,
        ..
      } = parts;
      let _ =
        state.with(|state| state.count(|counts| counts.moved = counts.moved.saturating_add(1)));
      // The seat here comes back before the connection leaves (the module doc).
      drop(seat);
      migrate::<S, M>(
        shard,
        Moving {
          stream: socket.stream.into_owned(),
          connection,
          tracker,
          make,
        },
      );
    }
  }
}

/// A connection on its way to the shard its repository lives on (the module doc): its socket's handle, its engine with
/// the head undecided, its waits, and what makes its site there. Each part is plain data, so it moves whole.
struct Moving<M> {
  /// The socket's handle, its readiness armed on no shard between awaits.
  stream: OwnedStream,
  /// The engine, its head whole and undecided.
  connection: Connection,
  /// Its waits.
  tracker: Tracker,
  /// What makes its site.
  make: M,
}

/// Moves `moving` to runtime shard `shard`: one spawn on that shard's control channel carrying the connection, which
/// there adopts the socket and is seated and served ([`arrived`]). A spawn the channel refuses dropped the handle with
/// it, so the connection closed; it is counted, as the NFS export counts a refused move.
fn migrate<S, M>(shard: u16, moving: Moving<M>)
where
  S: Site + 'static,
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

/// A connection arrived on this shard ([`migrate`]): its socket adopted here and its serving a task of this shard's,
/// which seats it ([`seat_arrival`]) and drives it on. Lost, and counted, on a shard keeping no edge state or where the
/// socket cannot be adopted.
fn arrived<S, M>(moving: Moving<M>)
where
  S: Site + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let Moving {
    stream,
    connection,
    tracker,
    make,
  } = moving;
  let (Some(state), Ok(stream)) = (here(), TcpStream::from_owned(stream)) else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return;
  };
  match slates_rt::futures::spawn(serve_arrival::<S, M>(
    state, stream, connection, tracker, make,
  )) {
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

/// Seats a connection that moved here and serves it on: its task is the seat's, so a reap here cancels it.
async fn serve_arrival<S, M>(
  state: Kept<EdgeState>,
  stream: TcpStream,
  connection: Connection,
  tracker: Tracker,
  make: M,
) where
  S: Site + 'static,
  M: Fn() -> S + Clone + Send + 'static,
{
  let Ok(slot) = seat_arrival(state).await else {
    let _ = state.with(|state| {
      state.count(|counts| counts.arrivals_refused = counts.arrivals_refused.saturating_add(1));
    });
    crate::fleet::count_refusal(ARRIVAL_REFUSED);
    return;
  };
  let Some(seat) = Seat::take(state, slot) else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return;
  };
  let _ = state.with(|state| {
    if let Some(task) = slates_rt::futures::current_task() {
      state.set_task(slot, task);
    }
    state.count(|counts| counts.arrived = counts.arrived.saturating_add(1));
  });
  drive(stream, connection, make, seat, tracker).await;
}

/// Why a connection that moved here was not seated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArrivalRefused {
  /// Every seat is taken and as many arrivals already wait as the shard has seats.
  Full,
  /// The edge's state is gone.
  Gone,
}

/// A seat for a connection that moved here (the module doc): a free one at once; with every seat taken, as a new
/// connection is seated by the listener ([`seat_for`]: the most overdue connection's, or the next to come back), while
/// fewer arrivals wait than the shard has seats; refused past that.
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

/// Counts how a connection ended: by its loop's reason, then by the engine's. Called before the connection's socket
/// closes, so its client cannot see the end before the count; a refusal was counted as it ended the connection
/// (`Parts::close_write`), a reap as it was chosen ([`EdgeState::seating`]).
fn count_end(state: Kept<EdgeState>, why: Why, ended: Option<Ended>) {
  if why == Why::Failed {
    crate::fleet::count_refusal(SOCKET_FAILED);
  }
  let failed = why == Why::Failed || matches!(ended, Some(Ended::Tls | Ended::Aborted));
  if failed {
    let _ =
      state.with(|state| state.count(|counts| counts.failed = counts.failed.saturating_add(1)));
  }
}

#[cfg(test)]
#[path = "vcs_http/tests/serve.rs"]
mod tests;
