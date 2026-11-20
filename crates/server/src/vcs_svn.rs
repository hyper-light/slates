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
//! **Where a URL leads** (A-695). The greeting is `svnserve`'s byte for byte but for `svndiff1`, which slates does not
//! announce, so no client sends it zlib (§4.17.4 "5. The wire"; A-599's handshake); the client's answer, its version,
//! capabilities and client strings, is judged by A-599's handshake; then its URL is resolved as `svnserve`'s
//! `find_repos` resolves it (`slates_svn::find`): no `svn://` URL, a path climbing with `..` and the server's root are
//! answered in `svnserve`'s words. Otherwise the repository is found as `svn_repos_find_root_path` finds it, walking up
//! from the whole path ([`locate`], which the `svn+ssh://` tunnel calls too): the path's leading runs of segments are
//! the names it could have, longest first, since a name may hold a `/`; each is asked of the shard whose partition its
//! name routes to (`verbs::owner_of_name`, as `CreateRepository` routes it, D-14), every shard once with all of its
//! candidates, and the longest held as a Subversion repository is the one. A repository another shard owns
//! moves the connection there whole, as the HTTP edge moves one ([`migrate`]): the socket's handle, the connection with
//! its answer read, its unread bytes and its deadline, one spawn on that shard's control channel; its seat here comes
//! back as it leaves, and there it takes a seat as a new connection would, a free one, the most overdue's, or a wait
//! bounded by the seats ([`seat_arrival`]). So a repository is found and served on the shard whose partition holds its
//! record and its store, read with no crossing.
//!
//! **Opened.** On the owner, no Subversion repository of the name is "No repository found in '…'" (210005). Its records
//! are read as the shared Subversion lender lends them there (`crate::vcs_store::svn_view::lend_in`, A-681): a
//! repository whose records the shard does not hold closes the connection, counted. Otherwise the repository is opened
//! with its UUID (also its realm, `svnserve`'s default), its root URL as `find_repos` derives it, and the mechanisms it
//! allows: none yet, since a client that has not authenticated is no principal (§4.13), so no repository reads
//! anonymously, and CRAM-MD5 waits on the keyed contexts of enrolled credentials (A-692). A repository that allows no
//! one is answered as `svnserve` answers one, "No access allowed to this repository" (170001), and the connection ends.
//! The authenticated session, each call's sources lent afresh by the same lender, follows once a mechanism does (GAPS,
//! A-656).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::task::{Poll, Waker};

use slates_db::catalog::VcsTool;
use slates_rt::futures::{cancel, detach, now_ns, spawn_child};
use slates_rt::shard::Kept;
use slates_rt::tcp::{OwnedStream, SocketAddrV4, TcpListener, TcpStream};
use slates_rt::{RtError, Runtime, ShardId, TaskId, futures};
use slates_svn::dump::Uuid;
use slates_svn::find::{Found, find_repository};
use slates_svn::handshake::{Access, Unserved};
use slates_svn::index::{ContentIdentity, Revision};
use slates_svn::report::exchange::Budget;
use slates_svn::serve::{Connection, Limits, Sources, Turn};

use crate::config::DaemonConfig;
use crate::vcs_http::waits::first_rto_ns;
use crate::vcs_http::{Bounds, awaited};
use crate::vcs_store::svn_view::lend_in;

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
/// Counter: a client's URL that leads to no repository, answered in `svnserve`'s words: no `svn://` URL, a path that
/// climbs with `..`, the server's root, or a name no Subversion repository has.
const UNSERVED: &str = "vcs_svn.unserved";
/// Counter: a repository that allows no one, answered "No access allowed to this repository".
const NO_ACCESS: &str = "vcs_svn.no_access";
/// Counter: a lookup a shard did not answer within the liveness budget: the connection closed, its repository unknown.
const LOOKUP_UNANSWERED: &str = "vcs_svn.lookup_unanswered";
/// Counter: a Subversion repository whose store its owner shard does not hold: the connection closed.
const STORE_MISSING: &str = "vcs_svn.store_missing";
/// Counter: a shard that could not keep its listener state, so a connection that moves there is lost.
const NOT_KEPT: &str = "vcs_svn.not_kept";
/// Counter: a move its target shard's control channel refused: the descriptor dropped with the spawn, so the
/// connection closed.
const MOVE_REFUSED: &str = "vcs_svn.move_refused";
/// Counter: a connection that arrived on a shard keeping no listener state, or whose socket could not be adopted.
const ARRIVAL_LOST: &str = "vcs_svn.arrival_lost";
/// Counter: a connection that arrived on a shard whose seats were all taken with as many arrivals already waiting as it
/// has seats: closed.
const ARRIVAL_REFUSED: &str = "vcs_svn.arrival_refused";
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
/// it holds, in names, and its root): the root alone, since no drive runs before a session is authenticated, which none
/// is yet (the module doc); the authenticated session takes the repository's own.
const ROOT_ALONE: usize = 1;

/// Whether a repository lets a client read without authenticating (`svnserve`'s `anon-access`): none does, since every
/// request is a principal's and a client that has not authenticated is none (§4.13).
const ANONYMOUS_READS: bool = false;

/// Whether a repository's users may authenticate with CRAM-MD5: not until the keyed contexts of enrolled credentials
/// are held (A-692), so no mechanism is offered and a repository answers as `svnserve` answers one that allows no one.
const CRAM_MD5_KEYS: bool = false;

/// Whether `repos-info` announces mergeinfo: every repository keeps it, as an FSFS repository of a format that records
/// mergeinfo announces it (`svn_repos_has_capability`; the recorded `anonymous` session's `( mergeinfo )`).
const MERGEINFO_KEPT: bool = true;

/// The repository a connection reads before its session is authenticated: none. No value of it exists, so no connection
/// is handed sources ([`Connection::receive`] with none), which is all the handshake takes; the authenticated session
/// is handed its repository's store by the shared Subversion lender and drops this.
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

/// A shard's listener state, shared by its connections and, on the listener's shard, the listener, through a [`Kept`]
/// handle: its seats, who waits for one, and the connections that moved here waiting. Every shard keeps one, so a
/// connection that moves to a repository's owner is seated there. Nothing in it is lent across a wait.
struct ListenerState {
  /// One entry for each connection served at once, `Some` while taken.
  seats: RefCell<Vec<Option<Taken>>>,
  /// Who waits for a seat: the listener holding an accepted connection, and connections that moved here, each once.
  waiting: RefCell<Vec<Waker>>,
  /// The connections that moved here and wait for a seat: at most as many as the shard has seats.
  arriving: Cell<usize>,
  /// How many seats there are: the bound on the arrivals waiting.
  seat_count: usize,
}

impl ListenerState {
  /// A listener serving `connections` at once.
  fn new(connections: usize) -> ListenerState {
    ListenerState {
      seats: RefCell::new(vec![None; connections]),
      waiting: RefCell::new(Vec::new()),
      arriving: Cell::new(0),
      seat_count: connections,
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
    self.wait_for_seat(waker);
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
    if arriving >= self.seat_count {
      return false;
    }
    self.arriving.set(arriving.saturating_add(1));
    true
  }

  /// One connection that waited here for a seat stopped waiting.
  fn arrival_seated(&self) {
    self.arriving.set(self.arriving.get().saturating_sub(1));
  }

  /// A free seat, if one is.
  fn free_seat(&self) -> Option<usize> {
    self
      .seats
      .try_borrow()
      .ok()?
      .iter()
      .position(Option::is_none)
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

thread_local! {
  /// This shard's listener state, once kept here ([`keep_here`]): where a connection that moves here is seated.
  static HERE: Cell<Option<Kept<ListenerState>>> = const { Cell::new(None) };
}

/// Keeps `state` on this shard for the daemon's life as the shard's listener state (the module doc): called once a
/// shard, at the daemon's start; refused off a shard's thread.
fn keep_here(state: ListenerState) -> Result<Kept<ListenerState>, RtError> {
  let kept = slates_rt::registry::with_current(|context| context.keep(state))
    .ok_or(RtError::NotOnShardThread)??;
  HERE.with(|here| here.set(Some(kept)));
  Ok(kept)
}

/// This shard's listener state, if one was kept here.
fn here() -> Option<Kept<ListenerState>> {
  HERE.with(Cell::get)
}

/// Starts the svn:// listener `config` names, if it names one (the module doc): every shard of `shards` keeps its
/// listener state and the `control` shard serves the listener. The port it listens on; none when none is asked for or
/// it did not start, which is logged.
pub(crate) fn start(
  runtime: &Runtime,
  control: ShardId,
  shards: &[ShardId],
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
  for shard in shards.iter().copied().filter(|shard| *shard != control) {
    runtime.spawn_on(shard, async move {
      if keep_here(ListenerState::new(bounds.connections)).is_err() {
        crate::fleet::count_refusal(NOT_KEPT);
      }
    })?;
  }
  runtime.spawn_on(control, async move {
    let Ok(state) = keep_here(ListenerState::new(bounds.connections)) else {
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
    let stream = listener.accept().await.map_err(ListenerRefused::Listener)?;
    let Ok(round_trip) = stream.round_trip() else {
      crate::fleet::count_refusal(NO_ROUND_TRIP);
      continue;
    };
    let bound_ns = first_rto_ns(
      round_trip.ns,
      bounds.granularity_ns().max(round_trip.resolution_ns),
    );
    let slot = seat_for(state).await?;
    let seat = Seat::take(state, slot).ok_or(ListenerRefused::StateGone)?;
    match spawn_child(converse(stream, seat, limits, bound_ns)) {
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum Next {
  /// Read more of the client's bytes.
  Read,
  /// Drop the first bytes of the input, taken, and call again.
  Consume(usize),
  /// Call again with the same input.
  Again,
  /// The client's answer was read, its first `consumed` bytes, and its URL leads to `found`: open it where its name
  /// lives.
  Locate {
    /// Where the URL leads.
    found: Found,
    /// The bytes the answer took.
    consumed: usize,
  },
  /// End the connection.
  End,
}

/// A connection's state that moves with it to its repository's owner (the module doc): plain data, so it moves whole.
struct Conversation {
  /// The connection, its answer read.
  connection: Connection,
  /// The client's bytes not yet taken.
  input: Vec<u8>,
  /// Its limits.
  limits: Limits,
  /// When a wait on the client becomes overdue.
  deadline_ns: u64,
}

/// What one step of a conversation leads to ([`step`]).
enum Stepped {
  /// It goes on here.
  Go,
  /// It ends.
  End,
  /// Its repository lives on another shard: it moves there.
  Elsewhere(Found, Located),
}

/// One connection, seated (the module doc): the greeting, then the conversation, each wait on the client marked overdue
/// past `bound_ns` from the greeting, and waited on.
async fn converse(stream: TcpStream, seat: Seat, limits: Limits, bound_ns: u64) {
  let mut out = Vec::new();
  let Ok(connection) = Connection::new(limits, &mut out) else {
    return;
  };
  if stream.write_all(&out).await.is_err() {
    return;
  }
  let conversation = Conversation {
    connection,
    input: Vec::new(),
    limits,
    deadline_ns: now_ns().saturating_add(bound_ns),
  };
  go_on(stream, conversation, seat).await;
}

/// Goes on with `conversation` over `stream` until it ends here or moves to the shard its repository's name lives on
/// ([`migrate`]), its seat here given back as it leaves.
async fn go_on(stream: TcpStream, mut conversation: Conversation, seat: Seat) {
  let mut out = Vec::new();
  loop {
    match step(&stream, &mut conversation, &mut out, &seat).await {
      Stepped::Go => {}
      Stepped::End => return,
      Stepped::Elsewhere(found, located) => {
        migrate(Moving {
          stream: stream.into_owned(),
          conversation,
          found,
          located,
        });
        return;
      }
    }
  }
}

/// One call of the conversation's connection and what follows it: its output written, then a read, a consumption, a
/// repository opened here, or the move to where its name lives.
async fn step(
  stream: &TcpStream,
  conversation: &mut Conversation,
  out: &mut Vec<u8>,
  seat: &Seat,
) -> Stepped {
  out.clear();
  let next = answer(
    &mut conversation.connection,
    &conversation.input,
    out,
    conversation.limits,
  );
  if !out.is_empty() && stream.write_all(out).await.is_err() {
    return Stepped::End;
  }
  match next {
    Next::Read => {
      let read = read_more(
        stream,
        &mut conversation.input,
        conversation.limits,
        conversation.deadline_ns,
        seat,
      )
      .await;
      if read { Stepped::Go } else { Stepped::End }
    }
    Next::Consume(taken) => {
      conversation
        .input
        .drain(..taken.min(conversation.input.len()));
      Stepped::Go
    }
    Next::Again => Stepped::Go,
    Next::End => Stepped::End,
    Next::Locate { found, consumed } => {
      conversation
        .input
        .drain(..consumed.min(conversation.input.len()));
      match locate(&found).await {
        Location::Found(located) if Some(located.shard) != this_shard() => {
          Stepped::Elsewhere(found, located)
        }
        Location::Found(located) => {
          opened(
            stream,
            &mut conversation.connection,
            &found,
            Some(&located),
            out,
          )
          .await
        }
        Location::Nowhere => opened(stream, &mut conversation.connection, &found, None, out).await,
        Location::Unanswered => {
          crate::fleet::count_refusal(LOOKUP_UNANSWERED);
          Stepped::End
        }
      }
    }
  }
}

/// What `connection` makes of the client's bytes `input`, its output appended to `out` (the module doc): before a
/// session is established the handshake asks for more bytes or hands over the client's answer, whose URL is resolved;
/// any other turn, and any refusal, ends the connection, the refusal's words already in `out`.
fn answer(connection: &mut Connection, input: &[u8], out: &mut Vec<u8>, limits: Limits) -> Next {
  let budget = Budget {
    room: limits.item_bytes,
    steps: limits.most_open,
  };
  match connection.receive(None::<&Sources<'_, Unresolved>>, input, out, budget) {
    Ok(Turn::NeedMore { .. }) => Next::Read,
    Ok(Turn::Took { consumed }) => Next::Consume(consumed),
    Ok(Turn::Wrote) => Next::Again,
    Ok(Turn::Opened { open, consumed }) => match find_repository(open.url) {
      Ok(found) => Next::Locate { found, consumed },
      Err(unserved) => {
        refuse(connection, unserved, out);
        Next::End
      }
    },
    Ok(_) | Err(_) => Next::End,
  }
}

/// Answers the client's URL with `unserved`, in `svnserve`'s words, appended to `out`; the connection ends.
fn refuse(connection: &mut Connection, unserved: Unserved<'_>, out: &mut Vec<u8>) {
  crate::fleet::count_refusal(UNSERVED);
  if connection.refuse(unserved, out).is_err() {
    crate::fleet::count_refusal(NOT_REFUSED);
  }
}

/// Where the repository a client's URL leads into lives ([`locate`]).
pub(crate) enum Location {
  /// Found.
  Found(Located),
  /// No candidate names a Subversion repository: "No repository found".
  Nowhere,
  /// A shard asked did not answer within the liveness budget, or the caller is on no shard: the repository is unknown.
  Unanswered,
}

/// A repository found for a client's URL: where it lives and how much of the URL's path its name takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Located {
  /// The runtime shard whose partition holds its record and its store.
  pub(crate) shard: u16,
  /// Its name.
  pub(crate) name: String,
  /// How many of the path's leading segments its name takes ([`Found::root_url`]).
  pub(crate) depth: usize,
}

/// Finds where the repository `found` leads into lives, as `svn_repos_find_root_path` finds its root (the module doc,
/// "Where a URL leads"): every candidate name asked of the shard its name routes to, each shard once with all of its
/// candidates, the longest name held as a Subversion repository the one. Called by the `svn://` listener and the
/// `svn+ssh://` tunnel alike, on the shard the connection is on.
pub(crate) async fn locate(found: &Found) -> Location {
  let Some((origin, asked)) = crate::state::with_state(|state| {
    let mut asked: BTreeMap<u16, Vec<(usize, String)>> = BTreeMap::new();
    for (depth, name) in found.candidates() {
      let partition = crate::verbs::owner_of_name(&name, state.shards.len());
      if let Some(shard) = crate::verbs::shard_of_partition(state, partition) {
        asked.entry(shard).or_default().push((depth, name));
      }
    }
    (state.shard, asked)
  }) else {
    return Location::Unanswered;
  };
  let mut deepest: Option<Located> = None;
  for (shard, names) in asked {
    let held = crate::xshard::call_in_shard_time_given_room(
      origin,
      shard,
      move |state| deepest_held(state, &names),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match held {
      None => return Location::Unanswered,
      Some(Some((depth, name))) if deepest.as_ref().is_none_or(|found| depth > found.depth) => {
        deepest = Some(Located { shard, name, depth });
      }
      Some(_) => {}
    }
  }
  deepest.map_or(Location::Nowhere, Location::Found)
}

/// The longest of `names`, each with the depth it takes, that this shard holds as a Subversion repository.
fn deepest_held(
  state: &mut crate::state::ShardState,
  names: &[(usize, String)],
) -> Option<(usize, String)> {
  let repositories = state.db.partition().repositories();
  names
    .iter()
    .filter(|(_, name)| {
      repositories
        .repository_named(name)
        .is_some_and(|record| record.tool == VcsTool::Subversion)
    })
    .max_by_key(|(depth, _)| *depth)
    .cloned()
}

/// The runtime shard this task runs on, if any.
fn this_shard() -> Option<u16> {
  crate::state::with_state(|state| state.shard)
}

/// What this shard holds of the repository a name names.
enum Held {
  /// A Subversion repository, its store's UUID.
  Repository(Uuid),
  /// A Subversion repository whose store this shard does not hold.
  NoStore,
  /// No Subversion repository of the name.
  None,
}

/// What this shard holds of repository `name` (the module doc, "Opened").
fn held(name: &str) -> Held {
  crate::state::with_state(|state| {
    let Some(id) = state
      .db
      .partition()
      .repositories()
      .repository_named(name)
      .filter(|record| record.tool == VcsTool::Subversion)
      .map(|record| record.id)
    else {
      return Held::None;
    };
    // The records as the shared lender lends them on their owner (`vcs_store::svn_view`), the UUID its text.
    match lend_in(state, id, |lent| Uuid::parse(lent.uuid.as_bytes())).flatten() {
      Some(uuid) => Held::Repository(uuid),
      None => Held::NoStore,
    }
  })
  .unwrap_or(Held::None)
}

/// Answers the client's URL on the shard that owns its repository (the module doc, "Opened"): the repository
/// `located` names opened with the mechanisms it allows, or, with none located or none held here any more, "No
/// repository found"; the answer written to `stream`. Whether the conversation goes on, which it does while the
/// client's mechanism is awaited.
async fn opened(
  stream: &TcpStream,
  connection: &mut Connection,
  found: &Found,
  located: Option<&Located>,
  out: &mut Vec<u8>,
) -> Stepped {
  out.clear();
  let go = match located.map(|located| (located, held(&located.name))) {
    Some((located, Held::Repository(uuid))) => open(connection, found, located.depth, uuid, out),
    Some((_, Held::NoStore)) => {
      crate::fleet::count_refusal(STORE_MISSING);
      false
    }
    Some((_, Held::None)) | None => {
      refuse(connection, Unserved::NoRepository { url: &found.url }, out);
      false
    }
  };
  if !out.is_empty() && stream.write_all(out).await.is_err() {
    return Stepped::End;
  }
  if go { Stepped::Go } else { Stepped::End }
}

/// Opens the repository of UUID `uuid` whose name takes the first `depth` segments of `found`'s path, with the
/// mechanisms it allows (the module doc, "Opened"), the answer appended to `out`: whether the client's mechanism is
/// awaited.
fn open(
  connection: &mut Connection,
  found: &Found,
  depth: usize,
  uuid: Uuid,
  out: &mut Vec<u8>,
) -> bool {
  let root_url = found.root_url(depth);
  let repository = slates_svn::handshake::Repository {
    uuid: uuid.as_bytes(),
    root_url: &root_url,
    realm: uuid.as_bytes(),
    mergeinfo: MERGEINFO_KEPT,
    anonymous: ANONYMOUS_READS,
    tunnel_user: None,
    cram_md5: CRAM_MD5_KEYS,
  };
  match connection.open(&repository, out) {
    Ok(Access::Asked) => true,
    Ok(Access::Denied) => {
      crate::fleet::count_refusal(NO_ACCESS);
      false
    }
    Err(_) => false,
  }
}

/// A connection on its way to the shard its repository lives on (the module doc): its socket's handle, its
/// conversation, where its URL leads and where its repository was found. Each part is plain data, so it moves whole.
struct Moving {
  /// The socket's handle, its readiness armed on no shard between awaits.
  stream: OwnedStream,
  /// The conversation, its answer read.
  conversation: Conversation,
  /// Where its URL leads.
  found: Found,
  /// Where its repository was found.
  located: Located,
}

/// Moves `moving` to its repository's shard: one spawn on that shard's control channel carrying the connection, which
/// there adopts the socket and is seated and served ([`arrived`]). A spawn the channel refuses dropped the handle with
/// it, so the connection closed; it is counted, as the HTTP edge counts a refused move.
fn migrate(moving: Moving) {
  let shard = moving.located.shard;
  let arrive = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      arrived(moving);
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
/// which seats it ([`seat_arrival`]) and opens its repository. Lost, and counted, on a shard keeping no listener state
/// or where the socket cannot be adopted.
fn arrived(moving: Moving) {
  let Moving {
    stream,
    conversation,
    found,
    located,
  } = moving;
  let (Some(state), Ok(stream)) = (here(), TcpStream::from_owned(stream)) else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return;
  };
  match futures::spawn(serve_arrival(state, stream, conversation, found, located)) {
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

/// Seats a connection that moved here, opens its repository and serves it on: its task is the seat's, so a reap here
/// cancels it.
async fn serve_arrival(
  state: Kept<ListenerState>,
  stream: TcpStream,
  mut conversation: Conversation,
  found: Found,
  located: Located,
) {
  let slot = match seat_arrival(state).await {
    Ok(slot) => slot,
    Err(ArrivalRefused::Full) => {
      crate::fleet::count_refusal(ARRIVAL_REFUSED);
      return;
    }
    Err(ArrivalRefused::Gone) => {
      crate::fleet::count_refusal(ARRIVAL_LOST);
      return;
    }
  };
  let Some(seat) = Seat::take(state, slot) else {
    crate::fleet::count_refusal(ARRIVAL_LOST);
    return;
  };
  if let Some(task) = futures::current_task() {
    let _ = state.with(|state| state.set_task(slot, task));
  }
  let mut out = Vec::new();
  let stepped = opened(
    &stream,
    &mut conversation.connection,
    &found,
    Some(&located),
    &mut out,
  )
  .await;
  if matches!(stepped, Stepped::Go) {
    go_on(stream, conversation, seat).await;
  }
}

/// Why a connection that moved here was not seated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArrivalRefused {
  /// Every seat is taken and as many arrivals already wait as the shard has seats.
  Full,
  /// The listener state is gone.
  Gone,
}

/// A seat for a connection that moved here (the module doc): a free one at once; with every seat taken, as the listener
/// seats a new connection ([`seat_for`]: the most overdue connection's, or the next to come back), while fewer arrivals
/// wait than the shard has seats; refused past that.
async fn seat_arrival(state: Kept<ListenerState>) -> Result<usize, ArrivalRefused> {
  let free = state
    .with(ListenerState::free_seat)
    .ok_or(ArrivalRefused::Gone)?;
  if let Some(slot) = free {
    return Ok(slot);
  }
  if !state
    .with(ListenerState::admit_arrival)
    .ok_or(ArrivalRefused::Gone)?
  {
    return Err(ArrivalRefused::Full);
  }
  let _waiting = Arriving(state);
  seat_for(state).await.map_err(|_| ArrivalRefused::Gone)
}

/// A connection that moved here, counted among the arrivals waiting for a seat for as long as it waits, however its wait
/// ends: seated, refused, or cancelled with its task.
struct Arriving(Kept<ListenerState>);

impl Drop for Arriving {
  fn drop(&mut self) {
    // Off its shard (the runtime gone) no listener state is left to count it in.
    let _ = self.0.with(ListenerState::arrival_seated);
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
