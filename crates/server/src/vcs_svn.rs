//! The svn:// listener on the daemon's runtime (A-689, A-656 part 2's listener; §4.17.4 piece 7, "5. The wire"; §4.3,
//! R6): a listener on the runtime's TCP, one connection a task, each running A-656's connection
//! (`slates_svn::serve::Connection`, the server's side of one ra_svn connection with no I/O) over its socket, as
//! `svnserve` serves `svn://`.
//!
//! **The start** ([`start`]). With an address in `DaemonConfig::vcs_svn`, the control shard serves the listener
//! ([`serve`]): the one a supervising anchor binds once at that address and holds across its daemons' restarts
//! (`slates_anchor::ENV_VCS_SVN`, as it holds the HTTP edge's), so the port never drops while a daemon restarts and a
//! client that connects meanwhile waits in its queue for the next daemon; else, with no anchor, one bound here
//! (`crate::daemon::held_or_bound_listener`). ra_svn has no TLS: CRAM-MD5 sends no password, but every other byte
//! crosses as it was written, so a daemon that is no fleet node serves `svn://` on a loopback address alone, the rule
//! the flag's reading holds it to before anything starts (`slates_api::role::vcs_address`) and the start checks again;
//! a fleet node's operator may publish it, as an operator publishes `svnserve`'s. An address off loopback on a daemon
//! that is no fleet node, one that does not bind, and a held listener somewhere else each leave the daemon serving
//! everything else, logged, as the HTTP edge's start does.
//!
//! **Bounds, derived** (the HTTP edge's, [`Bounds::of_shard`]): as many connections at once as the shard seats clients
//! (AC-2.6's admission limit, `DaemonConfig::clients_per_shard`), each of a client's items at most what one client's
//! region holds (`RegionGeometry::total_bytes`), and one read at most what one of `svnserve`'s reads takes
//! ([`READ_BYTES`]). The kernel's queue behind the listener is the same bound ([`Bounds::backlog`]).
//!
//! **A wait past its bound marks its connection overdue; the most overdue is reaped when a seat is needed** (the HTTP
//! edge's rule, main's ruling of 2026-11-19). The listener accepts the next connection whenever it comes; with every
//! seat taken it cancels the most overdue connection's task for it (its seat given back as the cancellation drops it),
//! and with none overdue it waits until a seat is given back or a connection becomes overdue. So a silent client never
//! holds a seat a new connection needs, and none is cut while seats are free. A wait's bound is RFC 6298 §2.2's first
//! RTO from the connection's own measurement, the TCP handshake's round trip as the kernel measured it at accept
//! ([`first_rto_ns`]), as the HTTP edge bounds a phase that has not learned yet. Learning each phase's wait, one
//! mechanism for the HTTP, SSH and svn:// edges, is owed (GAPS, A-656).
//!
//! **What is served.** The greeting, `svnserve`'s byte for byte, then the client's answer: its version, capabilities
//! and client strings judged by A-599's handshake, and its URL answered as `svnserve`'s `find_repos` answers a URL
//! under a root that holds no repository, "No repository found in '…'" (`SVN_ERR_RA_SVN_REPOS_NOT_FOUND`, 210005),
//! after which the connection ends, as `svnserve`'s does. No repository is looked up yet. The next piece ports the rest
//! of `find_repos` (the scheme's check, the URL's canonical form, a path that climbs with `..`), locates a repository's
//! name on its partition's shard, moves the connection there as the HTTP edge moves one, and lends the repository's
//! sources there with every call (GAPS, A-656).

use std::cell::RefCell;
use std::task::{Poll, Waker};

use slates_rt::futures::{cancel, detach, now_ns, spawn_child};
use slates_rt::shard::Kept;
use slates_rt::tcp::{SocketAddrV4, TcpListener, TcpStream};
use slates_rt::{RtError, Runtime, ShardId, TaskId, futures};
use slates_svn::handshake::Unserved;
use slates_svn::index::{ContentIdentity, Revision};
use slates_svn::report::exchange::Budget;
use slates_svn::serve::{Connection, Limits, Sources, Turn};

use crate::config::DaemonConfig;
use crate::vcs_http::waits::first_rto_ns;
use crate::vcs_http::{Bounds, awaited};

/// Counter: a listener the control shard could not keep the state of, or whose task its arena refused.
const NOT_STARTED: &str = "vcs_svn.not_started";
/// Counter: a task the runtime would not detach, so its slot is kept until the daemon's end.
const NOT_DETACHED: &str = "vcs_svn.not_detached";
/// Counter: an accepted connection whose round trip the kernel did not give: closed before its greeting.
const NO_ROUND_TRIP: &str = "vcs_svn.no_round_trip";
/// Counter: a connection's task its shard's arena refused: closed, its seat given back.
const SPAWN_REFUSED: &str = "vcs_svn.spawn_refused";
/// Counter: a connection whose wait on its client passed its bound: it waits on, overdue.
const OVERDUE: &str = "vcs_svn.overdue";
/// Counter: the most overdue connection reaped for a new one with every seat taken.
const REAPED: &str = "vcs_svn.reaped";
/// Counter: a reap whose cancellation the runtime refused.
const NOT_REAPED: &str = "vcs_svn.not_reaped";
/// Counter: a client's URL answered "No repository found", no repository being looked up yet.
const NO_REPOSITORY: &str = "vcs_svn.no_repository";
/// Counter: a refusal of a client's URL the connection would not write.
const NOT_REFUSED: &str = "vcs_svn.not_refused";

/// Format: `SVN_RA_SVN__PAGE_SIZE`, the page `svnserve` sizes a connection's buffers in (`libsvn_ra_svn/ra_svn.h` 65,
/// Subversion 1.15.0 and 1.14.x alike).
const SVN_PAGE_BYTES: usize = 4096;

/// Format: the pages in `SVN_RA_SVN__READBUF_SIZE`, `svnserve`'s per-connection read buffer (`ra_svn.h` 66).
const SVN_READ_PAGES: usize = 4;

/// Derived: the bytes one read takes from a connection's socket, `svnserve`'s own read buffer
/// (`SVN_RA_SVN__READBUF_SIZE`), so a read takes what one of `svnserve`'s would. A connection holds it beside its items.
const READ_BYTES: usize = SVN_READ_PAGES * SVN_PAGE_BYTES;

/// Derived: the nodes a drive holds open in the repository a connection resolves (`Limits::most_open`: the deepest path
/// it holds, in names, and its root): the root alone, since no repository is looked up yet and no drive runs (the module
/// doc); the lookup's piece takes the repository's own.
const ROOT_ALONE: usize = 1;

/// The repository a connection reads before its URL is resolved: none. No value of it exists, so no connection is
/// handed sources ([`Connection::receive`] with none), which is all the handshake takes; the lookup's piece hands the
/// repository's own green and drops this.
enum Unresolved {}

impl slates_svn::mergeinfo::Contents for Unresolved {
  fn entries(&self, _directory: &str, _revision: Revision) -> Option<Vec<String>> {
    match *self {}
  }

  fn mergeinfo_value(&self, _properties: &ContentIdentity) -> Option<&[u8]> {
    match *self {}
  }
}

impl slates_svn::reads::Repository for Unresolved {
  fn revision_properties(&self, _revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    match *self {}
  }

  fn properties(&self, _identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    match *self {}
  }

  fn text(&self, _identity: &ContentIdentity, _offset: u64) -> Option<&[u8]> {
    match *self {}
  }
}

impl slates_svn::commit::drive::Green for Unresolved {
  fn text(&self, _identity: &ContentIdentity) -> Option<&[u8]> {
    match *self {}
  }

  fn properties(&self, _identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    match *self {}
  }

  fn name_text(&self, _text: &[u8]) -> ContentIdentity {
    match *self {}
  }

  fn name_properties(&self, _properties: &[(Vec<u8>, Vec<u8>)]) -> ContentIdentity {
    match *self {}
  }
}

/// Why the listener stopped.
#[derive(Debug)]
enum ListenerRefused {
  /// Its socket failed.
  Listener(RtError),
  /// Its state is no longer kept on its shard (the runtime is ending).
  StateGone,
}

impl std::fmt::Display for ListenerRefused {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Listener(error) => write!(formatter, "its socket failed: {error}"),
      Self::StateGone => formatter.write_str("its state is gone from its shard"),
    }
  }
}

/// A taken seat: its connection's task once spawned, since when its wait has been past its bound, and whether it is
/// being reaped.
#[derive(Clone, Copy, Debug)]
struct Taken {
  /// The connection's task, once spawned.
  task: Option<TaskId>,
  /// The deadline its wait passed, while it waits past it.
  overdue_since_ns: Option<u64>,
  /// Whether it was chosen to be reaped: its seat comes back as its task's cancellation drops it.
  reaping: bool,
}

/// How the listener, holding an accepted connection, stands for a seat ([`ListenerState::seating`]).
enum Seating {
  /// This seat is free.
  Free(usize),
  /// None is free and this connection's task, the most overdue, is to be reaped for the one waiting (none when it was
  /// never spawned, whose seat comes back as its spawn is refused).
  Reap(Option<TaskId>),
  /// None is free and none is overdue: wait for a seat to come back or a connection to become overdue.
  Wait,
}

/// The listener's state on its shard, shared by it and its connections through a [`Kept`] handle: its seats, and the
/// listener while it waits for one. Nothing in it is lent across a wait.
struct ListenerState {
  /// One entry for each connection served at once, `Some` while taken.
  seats: RefCell<Vec<Option<Taken>>>,
  /// The listener's waker while it holds an accepted connection with every seat taken.
  waiting: RefCell<Option<Waker>>,
}

impl ListenerState {
  /// A listener serving `connections` at once.
  fn new(connections: usize) -> ListenerState {
    ListenerState {
      seats: RefCell::new(vec![None; connections]),
      waiting: RefCell::new(None),
    }
  }

  /// How the listener stands for a seat (the module doc): a free seat; or, with every seat taken, the most overdue
  /// connection not already being reaped, chosen now; or a wait. With none free, `waker` is kept to be woken when a seat
  /// comes back or a connection becomes overdue.
  fn seating(&self, waker: &Waker) -> Seating {
    let Ok(mut seats) = self.seats.try_borrow_mut() else {
      return Seating::Wait;
    };
    if let Some(free) = seats.iter().position(Option::is_none) {
      return Seating::Free(free);
    }
    if let Ok(mut waiting) = self.waiting.try_borrow_mut() {
      *waiting = Some(waker.clone());
    }
    let most_overdue = seats
      .iter_mut()
      .flatten()
      .filter(|taken| !taken.reaping)
      .filter_map(|taken| taken.overdue_since_ns.map(|since_ns| (since_ns, taken)))
      .min_by_key(|(since_ns, _)| *since_ns);
    match most_overdue {
      Some((_, taken)) => {
        taken.reaping = true;
        Seating::Reap(taken.task)
      }
      None => Seating::Wait,
    }
  }

  /// Takes seat `slot` for a connection just accepted.
  fn take_seat(&self, slot: usize) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(seat) = seats.get_mut(slot)
    {
      *seat = Some(Taken {
        task: None,
        overdue_since_ns: None,
        reaping: false,
      });
    }
  }

  /// Records the task serving seat `slot`.
  fn set_task(&self, slot: usize, task: TaskId) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(Some(taken)) = seats.get_mut(slot)
    {
      taken.task = Some(task);
    }
  }

  /// Marks seat `slot`'s connection overdue since `since_ns`, waking the listener, which may hold a connection waiting
  /// for a seat.
  fn mark(&self, slot: usize, since_ns: u64) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(Some(taken)) = seats.get_mut(slot)
      && !taken.reaping
    {
      taken.overdue_since_ns = Some(since_ns);
    }
    self.wake_listener();
  }

  /// Gives seat `slot` back, waking the listener if it waits for one.
  fn give_back(&self, slot: usize) {
    if let Ok(mut seats) = self.seats.try_borrow_mut()
      && let Some(seat) = seats.get_mut(slot)
    {
      *seat = None;
    }
    self.wake_listener();
  }

  /// Wakes the listener if it waits for a seat.
  fn wake_listener(&self) {
    let waker = self
      .waiting
      .try_borrow_mut()
      .ok()
      .and_then(|mut waiting| waiting.take());
    if let Some(waker) = waker {
      waker.wake();
    }
  }
}

/// A connection's seat: taken as it is given one and given back when this drops, however the connection ends (its end,
/// its task's cancellation by a reap or with its listener, a refused spawn), the listener woken if it waits for one.
struct Seat {
  /// The listener's state.
  state: Kept<ListenerState>,
  /// Which seat.
  slot: usize,
}

impl Seat {
  /// Takes seat `slot` of the listener `state` keeps; none off its shard.
  fn take(state: Kept<ListenerState>, slot: usize) -> Option<Seat> {
    state
      .with(|state| state.take_seat(slot))
      .map(|()| Seat { state, slot })
  }

  /// Marks the connection overdue since `since_ns`, its wait's deadline: it waits on.
  fn overdue(&self, since_ns: u64) {
    crate::fleet::count_refusal(OVERDUE);
    let slot = self.slot;
    // Off its shard (the runtime ending) no listener is left to tell.
    let _ = self.state.with(|state| state.mark(slot, since_ns));
  }
}

impl Drop for Seat {
  fn drop(&mut self) {
    let slot = self.slot;
    // Off its shard (the runtime ending) no listener is left to give the seat back to.
    let _ = self.state.with(|state| state.give_back(slot));
  }
}

/// Keeps `state` on this shard for the listener's life; refused off a shard's thread.
fn keep(state: ListenerState) -> Result<Kept<ListenerState>, RtError> {
  slates_rt::registry::with_current(|context| context.keep(state))
    .ok_or(RtError::NotOnShardThread)?
}

/// Starts the svn:// listener `config` names, if it names one (the module doc), on the `control` shard. The port it
/// listens on; none when none is asked for or it did not start, which is logged.
pub(crate) fn start(
  runtime: &Runtime,
  control: ShardId,
  config: &DaemonConfig,
) -> Result<Option<u16>, RtError> {
  let Some(address) = config.vcs_svn else {
    return Ok(None);
  };
  if config.fleet.is_none() && !address.ip().is_loopback() {
    return Ok(not_started(
      "svn:// is cleartext, which a daemon that is no fleet node serves on a loopback address alone",
    ));
  }
  let bounds = bounds_of(config);
  let room = usize::try_from(bounds.room).unwrap_or(usize::MAX);
  let limits = Limits {
    item_bytes: room,
    report_room: room,
    most_open: ROOT_ALONE,
  };
  let listener = match crate::daemon::held_or_bound_listener(
    slates_anchor::ENV_VCS_SVN,
    address,
    bounds.backlog(),
  ) {
    Ok(listener) => listener,
    Err(why) => return Ok(not_started(&why)),
  };
  let port = match listener.local_addr() {
    Ok(bound) => bound.port(),
    Err(error) => return Ok(not_started(&format!("its address is unread: {error}"))),
  };
  runtime.spawn_on(control, async move {
    slates_machine::log_line!("G234-DEBUG svn listener task runs, {} seats", bounds.connections);
    let Ok(state) = keep(ListenerState::new(bounds.connections)) else {
      slates_machine::log_line!("G234-DEBUG svn listener state not kept");
      crate::fleet::count_refusal(NOT_STARTED);
      return;
    };
    let served = futures::spawn(async move {
      if let Err(refused) = serve(listener, state, bounds, limits).await {
        slates_machine::log_line!("slates-server: the svn:// listener stopped: {refused}");
      }
    });
    match served {
      Ok(task) => {
        if detach(task).is_err() {
          crate::fleet::count_refusal(NOT_DETACHED);
        }
      }
      Err(_) => {
        crate::fleet::count_refusal(NOT_STARTED);
      }
    }
  })?;
  Ok(Some(port))
}

/// The listener's bounds on a shard of `config`: the HTTP edge's ([`Bounds::of_shard`]), its seats and room from the
/// shard's budget, its waits floored by the timer's tick.
fn bounds_of(config: &DaemonConfig) -> Bounds {
  Bounds::of_shard(
    config.clients_per_shard,
    config.region.total_bytes(),
    config.runtime.timer_tick_ns,
  )
}

/// Binds the listener at `address`, its backlog the listener's seats on a shard of `config`: the daemon's own, and the
/// one a supervising anchor binds once and holds for every daemon it starts (`crate::daemon::bind_vcs_svn_listener`),
/// so both queue alike.
pub(crate) fn bind(config: &DaemonConfig, address: SocketAddrV4) -> Result<TcpListener, RtError> {
  TcpListener::bind(address, bounds_of(config).backlog())
}

/// Logs why the listener did not start: no port to report. Logged, not counted: the shards' counters are not yet
/// serving.
fn not_started(why: &str) -> Option<u16> {
  slates_machine::log_line!("slates-server: the svn:// listener did not start: {why}");
  None
}

/// Serves `listener` until it fails or its task ends (the module doc): each connection a child task, seated first, its
/// waits bounded by its own round trip.
async fn serve(
  listener: TcpListener,
  state: Kept<ListenerState>,
  bounds: Bounds,
  limits: Limits,
) -> Result<(), ListenerRefused> {
  loop {
    slates_machine::log_line!("G234-DEBUG svn listener accepting");
    let stream = listener.accept().await.map_err(ListenerRefused::Listener)?;
    slates_machine::log_line!("G234-DEBUG svn listener accepted");
    let Ok(round_trip) = stream.round_trip() else {
      slates_machine::log_line!("G234-DEBUG svn no round trip");
      crate::fleet::count_refusal(NO_ROUND_TRIP);
      continue;
    };
    let bound_ns = first_rto_ns(
      round_trip.ns,
      bounds.granularity_ns().max(round_trip.resolution_ns),
    );
    slates_machine::log_line!("G234-DEBUG svn bound {bound_ns} ns; seating");
    let slot = seat_for(state).await?;
    slates_machine::log_line!("G234-DEBUG svn seated at {slot}");
    let seat = Seat::take(state, slot).ok_or(ListenerRefused::StateGone)?;
    let spawned = spawn_child(converse(stream, seat, limits, bound_ns));
    slates_machine::log_line!("G234-DEBUG svn spawned: {}", spawned.is_ok());
    match spawned {
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

/// Waits, holding an accepted connection, until a seat is free for it (the module doc): at once below the bound; with
/// every seat taken, by reaping the most overdue connection, or once a seat comes back or a connection becomes overdue.
async fn seat_for(state: Kept<ListenerState>) -> Result<usize, ListenerRefused> {
  std::future::poll_fn(|context| {
    match state.with(|state| state.seating(context.waker())) {
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
      None => Poll::Ready(Err(ListenerRefused::StateGone)),
    }
  })
  .await
}

/// What a connection does after one call of its [`Connection`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
  /// Read more of the client's bytes.
  Read,
  /// Drop the first bytes of the input, taken, and call again.
  Consume(usize),
  /// Call again with the same input.
  Again,
  /// End the connection.
  End,
}

/// One connection, seated (the module doc): the greeting, then the client's bytes read and handed to its
/// [`Connection`] until it ends; each wait on the client marked overdue past `bound_ns` from the greeting, and waited
/// on.
async fn converse(stream: TcpStream, seat: Seat, limits: Limits, bound_ns: u64) {
  slates_machine::log_line!("G234-DEBUG svn converse starts");
  let mut out = Vec::new();
  let Ok(mut connection) = Connection::new(limits, &mut out) else {
    slates_machine::log_line!("G234-DEBUG svn connection refused its limits");
    return;
  };
  let written = stream.write_all(&out).await;
  slates_machine::log_line!("G234-DEBUG svn greeting of {} bytes written: {:?}", out.len(), written);
  if written.is_err() {
    return;
  }
  let deadline_ns = now_ns().saturating_add(bound_ns);
  let mut input = Vec::new();
  loop {
    out.clear();
    let next = answer(&mut connection, &input, &mut out, limits);
    slates_machine::log_line!("G234-DEBUG svn next {next:?} with {} in, {} out", input.len(), out.len());
    if !out.is_empty() && stream.write_all(&out).await.is_err() {
      return;
    }
    match next {
      Next::Read => {
        if !read_more(&stream, &mut input, limits, deadline_ns, &seat).await {
          return;
        }
      }
      Next::Consume(taken) => {
        input.drain(..taken.min(input.len()));
      }
      Next::Again => {}
      Next::End => return,
    }
  }
}

/// What `connection` makes of the client's bytes `input`, its output appended to `out` (the module doc): before a
/// session is established the handshake asks for more bytes or hands over the client's answer, whose URL no repository
/// holds yet; any other turn, and any refusal, ends the connection, the refusal's words already in `out`.
fn answer(connection: &mut Connection, input: &[u8], out: &mut Vec<u8>, limits: Limits) -> Next {
  let budget = Budget {
    room: limits.item_bytes,
    steps: limits.most_open,
  };
  match connection.receive(None::<&Sources<'_, Unresolved>>, input, out, budget) {
    Ok(Turn::NeedMore { .. }) => Next::Read,
    Ok(Turn::Took { consumed }) => Next::Consume(consumed),
    Ok(Turn::Wrote) => Next::Again,
    Ok(Turn::Opened { open, .. }) => {
      crate::fleet::count_refusal(NO_REPOSITORY);
      if connection
        .refuse(Unserved::NoRepository { url: open.url }, out)
        .is_err()
      {
        crate::fleet::count_refusal(NOT_REFUSED);
      }
      Next::End
    }
    Ok(_) | Err(_) => Next::End,
  }
}

/// Reads more of the client's bytes onto `input`: at most one read's bytes and what the item's bound leaves, marking the
/// connection overdue once the wait passes `deadline_ns` and waiting on. Whether the client sent any: none at its end,
/// at its socket's failure, or with the item's bound reached.
async fn read_more(
  stream: &TcpStream,
  input: &mut Vec<u8>,
  limits: Limits,
  deadline_ns: u64,
  seat: &Seat,
) -> bool {
  let room = limits
    .item_bytes
    .saturating_sub(input.len())
    .min(READ_BYTES);
  if room == 0 {
    return false;
  }
  let start = input.len();
  input.resize(start.saturating_add(room), 0);
  let Some(buffer) = input.get_mut(start..) else {
    return false;
  };
  let read = awaited(stream.read(buffer), deadline_ns, || {
    seat.overdue(deadline_ns)
  })
  .await;
  let taken = match read {
    Ok((Ok(taken), _)) => taken,
    Ok((Err(_), _)) | Err(_) => 0,
  };
  input.truncate(start.saturating_add(taken));
  taken > 0
}

#[cfg(test)]
mod tests;
