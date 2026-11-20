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
//! with its UUID (also its realm, `svnserve`'s default), its root URL as `find_repos` derives it, and CRAM-MD5 alone: a
//! client that has not authenticated is no principal (§4.13), so no repository reads anonymously. The client names an
//! enrolled token by its id in hex and answers the challenge with the token as its password; the key is the one the
//! token's record keeps (A-692, `crate::credentials::cram_md5_key`), the session's principal the token's, and a user no
//! live token's record keys is answered as `svnserve` answers a user it has no password for. From there each call is
//! handed the repository's sources, lent afresh on the owner, with the principal's access: write where the repository's
//! policy lets it reach the repository at all, nothing where not (`vcs_policy::reaches`). A commit's drive runs over
//! those sources; a hook it waits for passes where the repository configures none at its point and is refused where one
//! is (none runs yet); its accepted work lands through the owner's commit step (A-698's
//! `repository::svn_commit::commit_work`, which the `svn+ssh://` tunnel calls too), and `close-edit` is answered with
//! `commit-info` or the step's refusal.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::task::{Poll, Waker};

use slates_db::catalog::{Principal, RepositoryId, VcsTool};
use slates_rt::futures::{cancel, detach, now_ns, spawn_child};
use slates_rt::shard::Kept;
use slates_rt::tcp::{OwnedStream, SocketAddrV4, TcpListener, TcpStream};
use slates_rt::{RtError, Runtime, ShardId, TaskId, futures};
use slates_svn::commit::drive::{Due, Work};
use slates_svn::dump::Uuid;
use slates_svn::find::{Found, find_repository};
use slates_svn::handshake::Verdict;
use slates_svn::handshake::{Access, Unserved};
use slates_svn::index::{ContentIdentity, Revision};
use slates_svn::lock::AprTime;
use slates_svn::migration::Repository as SvnRecords;
use slates_svn::report::exchange::Budget;
use slates_svn::serve::{CommitStart, Connection, Limits, Made, Policy, Sources, Turn};

use slates_vfs::clock::Clock;

use crate::config::DaemonConfig;
use crate::repository::svn_commit;
use crate::vcs_hooks::{self, CommittedView, HookPoint, LockToken, PendingView};
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

/// Format: the fewest bytes a path spends on one of its names: a name of one byte and its `/`.
const NAME_BYTES: usize = 2;

/// The rights some name of a repository must grant an authenticated principal for its session to write: any of the four,
/// each command then judged by the session, as the `svn+ssh://` tunnel opens one.
const SESSION_RIGHTS: [crate::vcs_policy::Right; 4] = [
  crate::vcs_policy::Right::Read,
  crate::vcs_policy::Right::Submit,
  crate::vcs_policy::Right::Rewrite,
  crate::vcs_policy::Right::Delete,
];

/// Whether a repository lets a client read without authenticating (`svnserve`'s `anon-access`): none does, since every
/// request is a principal's and a client that has not authenticated is none (§4.13).
const ANONYMOUS_READS: bool = false;

/// Whether a repository's users may authenticate with CRAM-MD5: every repository's, a user naming an enrolled token by
/// its id, the key the token's record keeps (A-692, `crate::credentials::cram_md5_key`).
const CRAM_MD5_KEYS: bool = true;

/// The host a CRAM-MD5 challenge names (`cram.c` `make_nonce`'s `<nonce.time@host>`): any text a client echoes back
/// in its digest; `svnserve` writes its own host name, which a client never reads.
const CHALLENGE_HOST: &std::ffi::CStr = c"slates";

/// Format: nanoseconds in a microsecond, the unit of the challenge's time (`apr_time_now`).
const NANOS_PER_MICRO: i64 = 1_000;

/// Counter: a lock verb's entries the owner's step refused, answered in each judged path's place.
const LOCKS_REFUSED: &str = "vcs_svn.locks_refused";
/// Counter: a conversation's turn begun in the same poll of its shard as the turn before it, no other task run between
/// them ([`go_on`]): a bug signal, a long answer or pipelined commands holding the shard (the git/git `svnrdump` spin).
pub(crate) const TURNS_UNYIELDED: &str = "vcs_svn.turns_unyielded";

/// Counter: a lock verb whose new token the system's generator could not draw: its landing refused.
const TOKEN_UNDRAWN: &str = "vcs_svn.token_undrawn";

/// Format: `SVN_ERR_FS_LOCK_OPERATION_FAILED` (`svn_error_codes.h`: the filesystem category's start, 160,000, plus 59).
const LOCK_OPERATION_FAILED: u32 = 160_059;

/// Counter: an accepted work the owner's commit step refused, answered as `close-edit`'s failure.
const COMMIT_REFUSED: &str = "vcs_svn.commit_refused";

/// Format: `SVN_ERR_UNSUPPORTED_FEATURE`, the miscellaneous category's start (200,000) plus 7 (`svn_error_codes.h`).
const UNSUPPORTED_FEATURE: u32 = 200_007;

/// The words a path is refused with where its pre-lock hook wrote more before any NUL than the server keeps of its
/// output: the token is not all here, so no lock is made with it (`vcs_hooks::SvnLockToken::PastCap`).
const TOKEN_PAST_CAP_WORDS: &str =
  "The pre-lock hook's token is longer than the server keeps of its output";

/// Format: the capability start-commit is given where the client announced it (`serve.c` remembers `mergeinfo` alone
/// among the client's capabilities, `svn_repos_remember_client_capabilities`; the recorded runs, A-623).
pub(crate) const MERGEINFO: &str = "mergeinfo";

/// Format: where a Subversion hook runs (`HookPoint::directory`: the filesystem's root, as `svnserve -d` runs them), the
/// view root the driver is handed, which no Subversion point reads.
#[cfg(unix)]
const HOOK_DIRECTORY: &str = "/";

/// The words a change is refused with at a point where the repository configures a hook this platform cannot run (no
/// hook driver on Windows yet), or whose repository left the shard, as the `svn+ssh://` tunnel refuses it.
const HOOK_NOT_RUN_WORDS: &str = "This server does not run this repository's hooks yet";

/// Counter: a CRAM-MD5 response naming no key a token's record holds: answered as `svnserve` answers a user it has
/// no password for.
const UNKEYED: &str = "vcs_svn.unkeyed";

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

  fn text(&self, _identity: &ContentIdentity, _offset: u64) -> Option<std::borrow::Cow<'_, [u8]>> {
    match *self {}
  }
}

impl slates_svn::commit::drive::Green for Unresolved {
  fn text(&self, _identity: &ContentIdentity) -> Option<std::borrow::Cow<'_, [u8]>> {
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
    // Derived: the nodes a drive holds open (`Limits::most_open`), the deepest path the repository holds and its
    // root: no path a client names or a commit adds holds more names than one item can carry.
    most_open: (room / NAME_BYTES).saturating_add(1),
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
  /// The client chose CRAM-MD5, its first `consumed` bytes: challenge it.
  Challenge(usize),
  /// The client's CRAM-MD5 response named `user`, its first `consumed` bytes: answer with that user's key.
  Key {
    /// The user, a token's id in hex.
    user: Vec<u8>,
    /// The bytes the response took.
    consumed: usize,
  },
  /// The client asked for a commit, its first `consumed` bytes: begin it over the repository's sources.
  CommitAsked(usize),
  /// The commit's drive waits for `due`'s hook, its first `consumed` bytes taken.
  Hook {
    /// The hook.
    due: Due,
    /// The bytes taken.
    consumed: usize,
  },
  /// The client asked for a lock verb, its first `consumed` bytes: judge it, land its entries, answer it.
  LockAsked(usize),
  /// The client asked for a revision property change past its checks, its first `consumed` bytes: run its hooks
  /// around its record.
  RevisionProperty {
    /// The pre-hook's call.
    call: Box<slates_svn::serve::RevisionPropertyCall>,
    /// The bytes taken.
    consumed: usize,
  },
  /// The commit's drive accepted `work` at `close-edit`, its first `consumed` bytes taken: land it.
  Accepted {
    /// The work.
    work: Box<Work>,
    /// The bytes taken.
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
  /// The repository opened, once it is, on its owner shard.
  opened: Option<Opened>,
  /// The principal the client's token proved, once it authenticated.
  principal: Option<Principal>,
  /// The commits the connection began, which name their transactions.
  commits: u64,
}

/// The repository a conversation opened: its id and name on the owner shard.
struct Opened {
  /// Its id.
  id: RepositoryId,
  /// Its name, which its green goes by in FSFS's messages.
  name: String,
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
    opened: None,
    principal: None,
    commits: 0,
  };
  go_on(stream, conversation, seat).await;
}

/// Goes on with `conversation` over `stream` until it ends here or moves to the shard its repository's name lives on
/// ([`migrate`]), its seat here given back as it leaves.
async fn go_on(stream: TcpStream, mut conversation: Conversation, seat: Seat) {
  let mut out = Vec::new();
  let mut last_poll = None;
  loop {
    // The shard's polls so far: unchanged since the last turn began, this turn follows it with nothing run between.
    let poll = futures::polls_so_far();
    if poll.is_some() && poll == last_poll {
      let _ = crate::state::with_state(|state| state.count(TURNS_UNYIELDED, 1));
    }
    last_poll = poll;
    let went = step(&stream, &mut conversation, &mut out, &seat).await;
    // A turn that never waited yields before the next: a long answer (a replay-range over a whole history, a checkout)
    // or commands pipelined in one read are many turns with no wait between, and a client that drains the socket as
    // fast as it is written never makes a write wait either, so without the yield the shard never steps (git/git's
    // last 2,000 commits through `svnrdump dump`: "shard 0 is spinning: 3596 ms" after 53 MB).
    if matches!(went, Stepped::Go) && futures::polls_so_far() == poll {
      futures::yield_now().await;
    }
    match went {
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
  let next = {
    // A-740: a shard stuck answering a call (a replay, a log, an update's drive) names it.
    let _doing = slates_rt::futures::doing("vcs_svn::answer");
    answer(conversation, out)
  };
  if !out.is_empty() && stream.write_all(out).await.is_err() {
    return Stepped::End;
  }
  follow(stream, conversation, next, out, seat).await
}

/// What follows the call's output: `next` taken, the bytes it consumed dropped from the input; a commit, lock, hook or
/// landing answered here, its answer written ([`written`]).
async fn follow(
  stream: &TcpStream,
  conversation: &mut Conversation,
  next: Next,
  out: &mut Vec<u8>,
  seat: &Seat,
) -> Stepped {
  let went = match next {
    Next::Read => return read_step(stream, conversation, seat).await,
    Next::Consume(taken) => {
      consume(conversation, taken);
      return Stepped::Go;
    }
    Next::Again => return Stepped::Go,
    Next::End => return Stepped::End,
    Next::Challenge(consumed) => {
      consume(conversation, consumed);
      return challenge(stream, &mut conversation.connection, out).await;
    }
    Next::Key { user, consumed } => {
      consume(conversation, consumed);
      return keyed(stream, conversation, &user, out).await;
    }
    Next::Locate { found, consumed } => {
      consume(conversation, consumed);
      return located_step(stream, conversation, found, out).await;
    }
    Next::CommitAsked(consumed) => {
      consume(conversation, consumed);
      begin_commit(conversation, out)
    }
    Next::LockAsked(consumed) => {
      consume(conversation, consumed);
      lock(stream, conversation, out).await
    }
    Next::RevisionProperty { call, consumed } => {
      consume(conversation, consumed);
      revision_property(stream, conversation, &call, out).await
    }
    Next::Hook { due, consumed } => {
      consume(conversation, consumed);
      answer_hook(stream, conversation, &due, out).await
    }
    Next::Accepted { work, consumed } => {
      consume(conversation, consumed);
      let _doing = slates_rt::futures::doing("vcs_svn::land");
      land(stream, conversation, *work, out).await
    }
  };
  written(stream, went, out).await
}

/// Reads more of the client's bytes into the conversation's input: on, or the end if none came.
async fn read_step(stream: &TcpStream, conversation: &mut Conversation, seat: &Seat) -> Stepped {
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

/// Drops the first `bytes` of the conversation's input, which an answer took (all of it, if it holds fewer).
fn consume(conversation: &mut Conversation, bytes: usize) {
  conversation
    .input
    .drain(..bytes.min(conversation.input.len()));
}

/// After a URL resolved to `found`: its repository opened here, the move to the shard it lives on, or the end when the
/// lookup went unanswered.
async fn located_step(
  stream: &TcpStream,
  conversation: &mut Conversation,
  found: Found,
  out: &mut Vec<u8>,
) -> Stepped {
  match locate(&found).await {
    Location::Found(located) if Some(located.shard) != this_shard() => {
      Stepped::Elsewhere(found, located)
    }
    Location::Found(located) => opened(stream, conversation, &found, Some(&located), out).await,
    Location::Nowhere => opened(stream, conversation, &found, None, out).await,
    Location::Unanswered => {
      crate::fleet::count_refusal(LOOKUP_UNANSWERED);
      Stepped::End
    }
  }
}

/// What the conversation's connection makes of the client's bytes, its output appended to `out` (the module doc):
/// before a session is established the handshake asks for more bytes, hands over the client's answer, whose URL is
/// resolved, or asks for a CRAM-MD5 challenge or key; once the client authenticated, each call is handed its
/// repository's sources, lent afresh on the owner ([`lend_in`]). A commit, and any refusal, ends the connection, the
/// refusal's words already in `out`.
fn answer(conversation: &mut Conversation, out: &mut Vec<u8>) -> Next {
  let budget = Budget {
    room: conversation.limits.item_bytes,
    steps: conversation.limits.most_open,
  };
  let Conversation {
    connection,
    input,
    opened,
    principal,
    ..
  } = conversation;
  let (Some(opened), Some(principal)) = (opened.as_ref(), principal.as_ref()) else {
    let turn = connection.receive(None::<&Sources<'_, Unresolved>>, input, out, budget);
    return next_of(connection, turn, out);
  };
  let lent = lent_sources(opened, principal, |sources| {
    let turn = connection.receive(Some(sources), input, out, budget);
    next_of(connection, turn, out)
  });
  lent.unwrap_or_else(|| {
    crate::fleet::count_refusal(STORE_MISSING);
    Next::End
  })
}

/// Runs `step` over the sources of the repository `opened` names, lent afresh on its owner ([`lend_in`]) with
/// `principal`'s access; none when the shard holds its records no longer.
fn lent_sources<T>(
  opened: &Opened,
  principal: &Principal,
  step: impl FnOnce(&Sources<'_, SvnRecords>) -> T,
) -> Option<T> {
  crate::state::with_state(|state| {
    let authenticated = authenticated_access(state, &opened.name, principal);
    lend_in(state, opened.id, |lent| {
      step(&Sources {
        index: lent.index,
        repository: lent.repository,
        locks: lent.locks,
        now: lent.now,
        policy: Policy {
          anonymous: slates_svn::authz::Access::None,
          authenticated,
          authz: None,
          filesystem: &opened.name,
        },
      })
    })
  })
  .flatten()
}

/// What an authenticated `principal` may do in repository `name` (`svnserve`'s `auth-access`): write where the
/// repository's policy lets it reach the repository at all (A-597's `vcs_policy::reaches`; each command then judged by
/// the session and, for a commit, by the owner's step), nothing where it does not.
fn authenticated_access(
  state: &crate::state::ShardState,
  name: &str,
  principal: &Principal,
) -> slates_svn::authz::Access {
  let reaches = state
    .db
    .partition()
    .repositories()
    .repository_named(name)
    .is_some_and(|record| {
      matches!(
        crate::vcs_policy::reaches(record, principal, &SESSION_RIGHTS),
        Ok(true)
      )
    });
  if reaches {
    slates_svn::authz::Access::Write
  } else {
    slates_svn::authz::Access::None
  }
}

/// What one of the connection's turns leads to (the module doc).
fn next_of(
  connection: &mut Connection,
  turn: Result<Turn<'_>, slates_svn::serve::Refused>,
  out: &mut Vec<u8>,
) -> Next {
  match turn {
    Ok(Turn::NeedMore { .. }) => Next::Read,
    Ok(
      Turn::Took { consumed }
      | Turn::Established { consumed, .. }
      | Turn::Unserved { consumed, .. },
    ) => Next::Consume(consumed),
    Ok(Turn::Wrote) => Next::Again,
    Ok(Turn::Opened { open, consumed }) => match find_repository(open.url) {
      Ok(found) => Next::Locate { found, consumed },
      Err(unserved) => {
        refuse(connection, unserved, out);
        Next::End
      }
    },
    Ok(Turn::ChallengeNeeded { consumed }) => Next::Challenge(consumed),
    Ok(Turn::KeyNeeded { user, consumed }) => Next::Key {
      user: user.to_vec(),
      consumed,
    },
    Ok(Turn::CommitAsked { consumed }) => Next::CommitAsked(consumed),
    Ok(Turn::LockAsked { consumed }) => Next::LockAsked(consumed),
    Ok(Turn::RevisionPropertyHook { call, consumed }) => Next::RevisionProperty { call, consumed },
    Ok(Turn::Hook { due, consumed }) => Next::Hook { due, consumed },
    Ok(Turn::Accepted { work, consumed }) => Next::Accepted { work, consumed },
    Err(_) => Next::End,
  }
}

/// Writes `out` to `stream` after a step that `went` on or not: whether the conversation goes on.
async fn written(stream: &TcpStream, went: bool, out: &[u8]) -> Stepped {
  if !went || (!out.is_empty() && stream.write_all(out).await.is_err()) {
    return Stepped::End;
  }
  Stepped::Go
}

/// Begins the commit the client asked for over the repository's sources, its transaction named as the `svn+ssh://`
/// tunnel names it (the connection's commits counted on the youngest revision), its new content bounded by one item's
/// room: whether the conversation goes on, the answer in `out`.
fn begin_commit(conversation: &mut Conversation, out: &mut Vec<u8>) -> bool {
  conversation.commits = conversation.commits.saturating_add(1);
  let (commits, room) = (conversation.commits, conversation.limits.item_bytes);
  let Conversation {
    connection,
    opened,
    principal,
    ..
  } = conversation;
  let (Some(opened), Some(principal)) = (opened.as_ref(), principal.as_ref()) else {
    return false;
  };
  let begun = lent_sources(opened, principal, |sources| {
    let start = CommitStart {
      transaction: crate::vcs_ssh::svn::transaction_name(sources.index.youngest(), commits),
      now: sources.now,
      room,
    };
    connection.commit(start, sources, out)
  });
  matches!(begun, Some(Ok(())))
}

/// Judges the lock verb the client asked for over the repository's sources on its owner, lands the lock entries it
/// makes through the owner's step (a `Record::Locks`, durable as a revision is; `repository::svn_commit::record`), and
/// answers it (A-693 part 2; `slates_svn::serve` `locks`), running the repository's hooks where `svnserve` runs them
/// (A-730, `fs-wrap.c`): pre-lock or pre-unlock for each path the session may write, before it is judged (a refusal is
/// that path's answer; pre-lock's token is the lock's), then post-lock or post-unlock once over the paths changed, its
/// failure the verb's end, the locks standing. Whether the conversation goes on, the answer in `out`. Each new token is
/// drawn from the system's generator; one it cannot give refuses the landing. The table holds at most as many locks as
/// one item has bytes.
async fn lock(stream: &TcpStream, conversation: &mut Conversation, out: &mut Vec<u8>) -> bool {
  let room = conversation.limits.item_bytes;
  let Some(hooked) = conversation.opened.as_ref().map(Opened::hooked) else {
    return false;
  };
  let called = {
    let Conversation {
      connection,
      opened,
      principal,
      ..
    } = &mut *conversation;
    let (Some(opened), Some(principal)) = (opened.as_ref(), principal.as_ref()) else {
      return false;
    };
    lent_sources(opened, principal, |sources| {
      connection.lock_verb(sources, out)
    })
  };
  let call = match called {
    Some(Ok(Some(call))) => call,
    Some(Ok(None)) => return true,
    Some(Err(_)) | None => return false,
  };
  let Some(verdicts) = pre_lock_hooks(HookSite::Listener(stream), &hooked, room, &call).await
  else {
    return false;
  };
  let undrawn = Cell::new(false);
  let mut draw = || {
    let mut token = [0u8; slates_svn::lock::UUID_BYTES];
    if aws_lc_rs::rand::fill(&mut token).is_err() {
      undrawn.set(true);
    }
    token
  };
  let judged = {
    let Conversation {
      connection,
      opened,
      principal,
      ..
    } = &mut *conversation;
    let (Some(opened), Some(principal)) = (opened.as_ref(), principal.as_ref()) else {
      return false;
    };
    lent_sources(opened, principal, |sources| {
      connection.lock_judged(sources, verdicts, &mut draw, room)
    })
  };
  let entries = match judged {
    Some(Ok(entries)) => entries,
    Some(Err(_)) | None => return false,
  };
  let landed = land_locks(conversation, entries, undrawn.get());
  let post = match conversation.connection.locks_landed(landed, out) {
    Ok(post) => post,
    Err(_) => return false,
  };
  // The paths answered go out before the post-hook runs, as `svnserve`'s callbacks write them (`lock_many_cb`).
  if !out.is_empty() && stream.write_all(out).await.is_err() {
    return false;
  }
  out.clear();
  let ended = match post {
    Some(call) => match post_lock_hook(HookSite::Listener(stream), &hooked, room, &call).await {
      Some(verdict) => verdict,
      None => return false,
    },
    None => Ok(()),
  };
  conversation.connection.locks_finished(ended, out).is_ok()
}

/// Lands a lock verb's `entries` through the owner's step (a `Record::Locks`): `Ok` for none, or the step's refusal;
/// refused where a new token could not be drawn (`undrawn`).
fn land_locks(
  conversation: &Conversation,
  entries: Vec<slates_svn::lock::LockEntry>,
  undrawn: bool,
) -> Result<(), (u32, String)> {
  let (Some(opened), Some(principal)) = (
    conversation.opened.as_ref(),
    conversation.principal.as_ref(),
  ) else {
    return Err(svn_commit::Uncommitted::NotHeld.failure());
  };
  if undrawn {
    crate::fleet::count_refusal(TOKEN_UNDRAWN);
    return Err((
      LOCK_OPERATION_FAILED,
      "No lock token could be drawn".to_owned(),
    ));
  }
  if entries.is_empty() {
    return Ok(());
  }
  land_record(
    (principal, opened.id),
    slates_svn::commit::record::Record::Locks(entries),
  )
  .inspect_err(|_| {
    crate::fleet::count_refusal(LOCKS_REFUSED);
  })
}

/// Lands `record` on `repository`'s logs by `principal` through the owner's step (`svn_commit::record`): `Ok`, or the
/// step's refusal as the connection answers it.
fn land_record(
  (principal, repository): (&Principal, RepositoryId),
  record: slates_svn::commit::record::Record,
) -> Result<(), (u32, String)> {
  crate::state::with_state(|state| svn_commit::record(state, (principal, repository), record))
    .unwrap_or(Err(svn_commit::Uncommitted::NotHeld))
    .map_err(|refused| refused.failure())
}

/// Runs the client's revision property change past its checks (A-702 part 2; `fs-wrap.c`
/// `svn_repos_fs_change_rev_prop4`), `call` its pre-hook's: pre-revprop-change, whose refusal (165006 where none is
/// configured) is the answer; then the change judged over the repository's sources and its record landed through the
/// owner's step with no wait between (`Record::RevisionProperties`), so no other change lands in between; then
/// post-revprop-change with the old value, its verdict the command's end. Whether the conversation goes on, the answer in
/// `out`; a client that left while a hook ran ends it.
async fn revision_property(
  stream: &TcpStream,
  conversation: &mut Conversation,
  call: &slates_svn::serve::RevisionPropertyCall,
  out: &mut Vec<u8>,
) -> bool {
  let room = conversation.limits.item_bytes;
  let Some(hooked) = conversation.opened.as_ref().map(Opened::hooked) else {
    return false;
  };
  let site = HookSite::Listener(stream);
  let Some(verdict) =
    revision_property_hook(site, &hooked, room, HookPoint::SvnPreRevpropChange, call).await
  else {
    return false;
  };
  let Conversation {
    connection,
    opened,
    principal,
    ..
  } = &mut *conversation;
  let (Some(opened), Some(principal)) = (opened.as_ref(), principal.as_ref()) else {
    return false;
  };
  let judged = lent_sources(opened, principal, |sources| {
    connection.revision_property_hooked(sources, verdict, out)
  });
  let record = match judged {
    Some(Ok(Some(record))) => record,
    Some(Ok(None)) => return true,
    Some(Err(_)) | None => return false,
  };
  let landed = land_record((principal, opened.id), record);
  let post = match connection.revision_property_landed(landed, out) {
    Ok(Some(post)) => post,
    Ok(None) => return true,
    Err(_) => return false,
  };
  let Some(ended) =
    revision_property_hook(site, &hooked, room, HookPoint::SvnPostRevpropChange, &post).await
  else {
    return false;
  };
  conversation
    .connection
    .revision_property_finished(ended, out)
    .is_ok()
}

/// Runs the revision-property hook at `point` (pre- or post-revprop-change) for `call` (A-702 part 2): its verdict, or
/// `None` where the client left before it ended.
pub(crate) async fn revision_property_hook(
  site: HookSite<'_>,
  hooked: &Hooked,
  words_cap: usize,
  point: HookPoint,
  call: &slates_svn::serve::RevisionPropertyCall,
) -> Option<Result<(), Vec<(u32, String)>>> {
  use slates_svn::serve::RevisionPropertyAction as Action;
  let action = match call.action {
    Action::Added => vcs_hooks::RevpropAction::Added,
    Action::Modified => vcs_hooks::RevpropAction::Modified,
    Action::Deleted => vcs_hooks::RevpropAction::Deleted,
  };
  let input = if point == HookPoint::SvnPreRevpropChange {
    vcs_hooks::svn_pre_revprop_change
  } else {
    vcs_hooks::svn_post_revprop_change
  };
  run_hook(site, hooked, words_cap, point, |env, repos| {
    let change = vcs_hooks::RevpropChange {
      repository: repos,
      revision: call.revision,
      author: &call.user,
      property: &call.name,
      action,
    };
    input(CommittedView(call.revision), env, change, &call.value)
  })
  .await
  .verdict
}

/// Runs the verb's pre-hook for each path in `call` in turn (A-730): each path's verdict, its refusal's chain or
/// pre-lock's token; `None` where the client left before a hook ended, which ends the conversation.
pub(crate) async fn pre_lock_hooks(
  site: HookSite<'_>,
  hooked: &Hooked,
  words_cap: usize,
  call: &slates_svn::serve::LockHookCall,
) -> Option<Vec<slates_svn::serve::PreHooked>> {
  let point = if call.locks {
    HookPoint::SvnPreLock
  } else {
    HookPoint::SvnPreUnlock
  };
  let mut verdicts = Vec::with_capacity(call.paths.len());
  for (path, token) in &call.paths {
    let ran = run_hook(site, hooked, words_cap, point, |env, repos| {
      let view = CommittedView(0);
      if call.locks {
        vcs_hooks::svn_pre_lock(
          view,
          env,
          repos,
          path,
          &call.user,
          &call.comment,
          call.force,
        )
      } else {
        vcs_hooks::svn_pre_unlock(view, env, repos, path, &call.user, token, call.force)
      }
    })
    .await;
    let verdict = ran.verdict?;
    verdicts.push(match (verdict, ran.token) {
      (Err(chain), _) => Err(chain),
      (Ok(()), Some(vcs_hooks::SvnLockToken::Given(token))) => {
        Ok(Some(String::from_utf8_lossy(&token).into_owned()))
      }
      (Ok(()), Some(vcs_hooks::SvnLockToken::PastCap)) => Err(vec![(
        LOCK_OPERATION_FAILED,
        TOKEN_PAST_CAP_WORDS.to_owned(),
      )]),
      (Ok(()), Some(vcs_hooks::SvnLockToken::Made) | None) => Ok(None),
    });
  }
  Some(verdicts)
}

/// Runs the verb's post-hook once over the paths it changed (A-730): `Ok`, or its failure's chain (`svn_told`: E165008
/// or E165009 over E165001); `None` where the client left before it ended.
pub(crate) async fn post_lock_hook(
  site: HookSite<'_>,
  hooked: &Hooked,
  words_cap: usize,
  call: &slates_svn::serve::LockHookCall,
) -> Option<Result<(), Vec<(u32, String)>>> {
  let point = if call.locks {
    HookPoint::SvnPostLock
  } else {
    HookPoint::SvnPostUnlock
  };
  let paths: Vec<&str> = call.paths.iter().map(|(path, _)| path.as_str()).collect();
  run_hook(site, hooked, words_cap, point, |env, repos| {
    let view = CommittedView(0);
    if call.locks {
      vcs_hooks::svn_post_lock(view, env, repos, &call.user, &paths)
    } else {
      vcs_hooks::svn_post_unlock(view, env, repos, &call.user, &paths)
    }
  })
  .await
  .verdict
}

/// Runs the hook the commit's drive waits for (A-730): start-commit at `open-root` with the user, the client's
/// capabilities and the transaction; pre-commit at `close-edit` with the transaction and the lock tokens the commit
/// presented; each over the repository's policy ([`run_hook`]). Its verdict resumes the drive, as `svnserve`'s does: on,
/// or refused in `check_hook_result`'s words. A client that left before the hook ended ends the conversation. A drive the
/// answer lets accept its work lands it ([`land`]).
async fn answer_hook(
  stream: &TcpStream,
  conversation: &mut Conversation,
  due: &Due,
  out: &mut Vec<u8>,
) -> bool {
  let Some(opened) = conversation.opened.as_ref().map(Opened::hooked) else {
    return false;
  };
  let (user, mergeinfo) = conversation
    .connection
    .session()
    .map(|session| {
      (
        hook_user(&session.principal),
        session.capabilities.mergeinfo,
      )
    })
    .unwrap_or_default();
  let tokens: Vec<LockToken> = conversation
    .connection
    .pending()
    .map(|pending| pending.lock_tokens())
    .unwrap_or_default()
    .into_iter()
    .map(|(path, token)| LockToken { path, token })
    .collect();
  let base = conversation
    .connection
    .pending()
    .map_or(0, |pending| pending.base);
  let words_cap = conversation.limits.item_bytes;
  let ran = match due {
    Due::StartCommit { transaction } => {
      let capabilities: &[&str] = if mergeinfo { &[MERGEINFO] } else { &[] };
      run_hook(
        HookSite::Listener(stream),
        &opened,
        words_cap,
        HookPoint::SvnStartCommit,
        |env, repos| {
          vcs_hooks::svn_start_commit(
            PendingView(base),
            env,
            repos,
            &user,
            capabilities,
            transaction,
          )
        },
      )
      .await
    }
    Due::PreCommit { transaction } => {
      run_hook(
        HookSite::Listener(stream),
        &opened,
        words_cap,
        HookPoint::SvnPreCommit,
        |env, repos| vcs_hooks::svn_pre_commit(PendingView(base), env, repos, transaction, &tokens),
      )
      .await
    }
  };
  let Some(answer) = ran.verdict else {
    return false;
  };
  let Conversation {
    connection,
    opened,
    principal,
    ..
  } = &mut *conversation;
  let (Some(opened), Some(principal)) = (opened.as_ref(), principal.as_ref()) else {
    return false;
  };
  let answered = lent_sources(opened, principal, |sources| {
    connection.answer_hook(answer, sources, out)
  });
  match answered {
    Some(Ok(Turn::Accepted { work, .. })) => land(stream, conversation, *work, out).await,
    Some(Ok(_)) => true,
    Some(Err(_)) | None => false,
  }
}

/// The user a Subversion hook is given: the session's, or empty for an anonymous one (`hooks.c` passes `""`).
fn hook_user(principal: &slates_svn::handshake::Principal) -> String {
  match principal {
    slates_svn::handshake::Principal::Anonymous => String::new(),
    slates_svn::handshake::Principal::User(name) => String::from_utf8_lossy(name).into_owned(),
  }
}

/// What a hook's run answers the conversation (A-730): the verdict an admission point resumes with, `None` where the
/// client left (nothing to resume, and the change does not go on); and post-commit's words, which `commit-info` carries
/// as `post-commit-err`.
#[derive(Debug, Default)]
pub(crate) struct HookAnswer {
  /// The verdict: on, or refused with errors outermost first.
  pub(crate) verdict: Option<Result<(), Vec<(u32, String)>>>,
  /// post-commit's failure, in `svnserve`'s words.
  pub(crate) post_commit_error: Option<String>,
  /// pre-lock's token: what its standard output asks the lock to be made with.
  pub(crate) token: Option<vcs_hooks::SvnLockToken>,
}

/// What a conversation's hooks need of its repository: its id, its name, and REPOS, the directory a hook is given (the
/// repository's name beneath `/`, as `SVNParentPath` lays repositories out, until A-645's read-only FSFS-layout view
/// gives `svnlook` a directory to read: design §4.17.4 item 9).
pub(crate) struct Hooked {
  /// The repository's id, which its hooks' learned times are kept under (unix alone, where hooks run).
  #[cfg(unix)]
  id: RepositoryId,
  /// Its name, which its record is found by on its owner shard.
  name: String,
  /// REPOS (unix alone, where hooks run).
  #[cfg(unix)]
  repos: String,
}

impl Hooked {
  /// What the hooks of the repository `id`, named `name`, need of it.
  pub(crate) fn new(id: RepositoryId, name: &str) -> Self {
    #[cfg(not(unix))]
    let _ = id;
    Self {
      #[cfg(unix)]
      id,
      name: name.to_owned(),
      #[cfg(unix)]
      repos: format!("/{name}"),
    }
  }
}

impl Opened {
  /// What this repository's hooks need of it.
  fn hooked(&self) -> Hooked {
    Hooked::new(self.id, &self.name)
  }
}

/// Where a hook's run is asked from (A-730): the `svn://` listener's connection, whose repository's policy is read on
/// its owner shard and whose client's hang-up ends the run; the `svn+ssh://` tunnel, which holds its repository's
/// record, its client's hang-up the SSH edge's signal (A-736); or the DAV site's exchange (A-733).
#[derive(Clone, Copy)]
pub(crate) enum HookSite<'a> {
  /// The listener's connection.
  Listener(&'a TcpStream),
  /// The tunnel's repository policy, and the SSH edge's signal of its client gone (A-736).
  Tunnel(
    &'a slates_db::catalog::RepositoryPolicy,
    crate::vcs_ssh::site::ClientGone<'a>,
  ),
  /// The DAV site's exchange (A-733): its client's hang-up and its repository's policy.
  Http(
    crate::vcs_http::ClientGone<'a>,
    &'a slates_db::catalog::RepositoryPolicy,
  ),
}

/// Runs the hook `repository`'s policy configures at `point` with the input `input` builds from the policy's hooks-env
/// and REPOS (A-730), asked from `site`: the listener's policy read on the owner shard first, a repository no longer
/// here refused as the commit step refuses it.
pub(crate) async fn run_hook(
  site: HookSite<'_>,
  repository: &Hooked,
  words_cap: usize,
  point: HookPoint,
  input: impl FnOnce(&vcs_hooks::SvnHooksEnv, &str) -> vcs_hooks::HookInput,
) -> HookAnswer {
  match site {
    HookSite::Listener(stream) => {
      let policy = crate::state::with_state(|state| {
        state
          .db
          .partition()
          .repositories()
          .repository_named(&repository.name)
          .map(|record| record.policy.clone())
      })
      .flatten();
      let Some(policy) = policy else {
        return HookAnswer {
          verdict: Some(Err(vec![(
            UNSUPPORTED_FEATURE,
            HOOK_NOT_RUN_WORDS.to_owned(),
          )])),
          post_commit_error: None,
          token: None,
        };
      };
      let gone = crate::vcs_http::ClientGone::watching(stream).gone();
      run_svn_hook(&policy, repository, words_cap, point, input, gone).await
    }
    HookSite::Tunnel(policy, gone) => {
      run_svn_hook(policy, repository, words_cap, point, input, gone.gone()).await
    }
    HookSite::Http(client, policy) => {
      run_svn_hook(policy, repository, words_cap, point, input, client.gone()).await
    }
  }
}

/// Runs the hook `policy` configures at `point` (`crate::svn_hooks::run`, the hook driver of A-622 with A-623's
/// Subversion rules), held to the bound its runs taught this shard and ended when `gone` answers (its client hung up),
/// then teaches the shard what this run showed. A point with no hook answers at once, running nothing,
/// as `svnserve` does.
#[cfg(unix)]
async fn run_svn_hook(
  policy: &slates_db::catalog::RepositoryPolicy,
  repository: &Hooked,
  words_cap: usize,
  point: HookPoint,
  input: impl FnOnce(&vcs_hooks::SvnHooksEnv, &str) -> vcs_hooks::HookInput,
  gone: impl core::future::Future<Output = ()>,
) -> HookAnswer {
  use crate::svn_hooks::{SvnHookSite, configured, run};
  let bound_ns = crate::state::with_state(|state| {
    configured(policy, point).and_then(|hook| state.hook_times.bound_ns(repository.id, hook))
  })
  .flatten();
  let env = vcs_hooks::SvnHooksEnv::of(policy);
  let input = input(&env, &repository.repos);
  let site = SvnHookSite {
    policy,
    view_root: std::path::Path::new(HOOK_DIRECTORY),
    words_cap,
  };
  let ran = run(&site, &input, bound_ns, gone).await;
  if let (Some(lesson), Some(hook)) = (ran.lesson, configured(policy, point)) {
    let _ = crate::state::with_state(|state| {
      state.hook_times.teach(repository.id, policy, hook, lesson);
    });
  }
  let post_commit_error = match &ran.told {
    vcs_hooks::SvnTold::PostCommitErr(words) => Some(words.clone()),
    vcs_hooks::SvnTold::Nothing | vcs_hooks::SvnTold::Failure(_) => None,
  };
  HookAnswer {
    verdict: ran.verdict(),
    post_commit_error,
    token: ran.token,
  }
}

/// No hook runs on Windows yet (the hook driver is Unix's alone, A-622 part 3): a point where the repository configures
/// one refuses its change in the not-run words, as before A-730, and a point with none lets it go on.
#[cfg(not(unix))]
async fn run_svn_hook(
  policy: &slates_db::catalog::RepositoryPolicy,
  _repository: &Hooked,
  _words_cap: usize,
  point: HookPoint,
  _input: impl FnOnce(&vcs_hooks::SvnHooksEnv, &str) -> vcs_hooks::HookInput,
  _gone: impl core::future::Future<Output = ()>,
) -> HookAnswer {
  let configured = policy
    .hooks
    .iter()
    .any(|hook| HookPoint::of(hook.point) == point);
  HookAnswer {
    verdict: Some(if configured {
      Err(vec![(UNSUPPORTED_FEATURE, HOOK_NOT_RUN_WORDS.to_owned())])
    } else {
      Ok(())
    }),
    post_commit_error: None,
    token: None,
  }
}

/// Lands the accepted `work` through the owner's commit step (A-698's `repository::svn_commit::commit_work`, which the
/// `svn+ssh://` tunnel calls too, at the owner's instant) and answers `close-edit` with `commit-info` or with the
/// step's refusal as its failure: whether the conversation goes on, the answer in `out`.
async fn land(
  stream: &TcpStream,
  conversation: &mut Conversation,
  work: Work,
  out: &mut Vec<u8>,
) -> bool {
  let (Some(opened), Some(principal)) = (
    conversation.opened.as_ref(),
    conversation.principal.as_ref(),
  ) else {
    return false;
  };
  let id = opened.id;
  let hooked = opened.hooked();
  let transaction = work.record.transaction.clone();
  let landed = crate::state::with_state(|state| {
    let now = AprTime::from_wall_ns(state.clock.wall_ns());
    svn_commit::commit_work(state, (principal, id), work, now)
  });
  let Some(landed) = landed else {
    return false;
  };
  // post-commit runs once the revision stands, and its failure is `commit-info`'s `post-commit-err` (A-730).
  let post_commit = match &landed {
    Ok(committed) => {
      let revision = committed.revision;
      let words_cap = conversation.limits.item_bytes;
      run_hook(
        HookSite::Listener(stream),
        &hooked,
        words_cap,
        HookPoint::SvnPostCommit,
        |env, repos| {
          vcs_hooks::svn_post_commit(CommittedView(revision), env, repos, revision, &transaction)
        },
      )
      .await
      .post_commit_error
    }
    Err(_) => None,
  };
  let answered = match &landed {
    // A client's work always carries the date the step gave it (`Revision::of_work`).
    Ok(committed) => conversation.connection.committed(
      &Made {
        revision: committed.revision,
        date: committed.date.as_deref().unwrap_or_default(),
        author: committed.author.as_deref(),
        post_commit_error: post_commit.as_deref(),
      },
      out,
    ),
    Err(refused) => {
      crate::fleet::count_refusal(COMMIT_REFUSED);
      conversation
        .connection
        .not_committed(&refused.answered(), out)
    }
  };
  answered.is_ok()
}

/// Challenges the client's CRAM-MD5 (`cram.c` `make_nonce`): a random number, the owner's wall clock in microseconds
/// and [`CHALLENGE_HOST`], the challenge written to `stream`.
async fn challenge(stream: &TcpStream, connection: &mut Connection, out: &mut Vec<u8>) -> Stepped {
  out.clear();
  let mut nonce = [0u8; size_of::<u64>()];
  let micros = crate::state::with_state(|state| state.clock.wall_ns() / NANOS_PER_MICRO);
  let (Ok(()), Some(micros)) = (aws_lc_rs::rand::fill(&mut nonce), micros) else {
    return Stepped::End;
  };
  if connection
    .challenge(u64::from_le_bytes(nonce), micros, CHALLENGE_HOST, out)
    .is_err()
    || stream.write_all(out).await.is_err()
  {
    return Stepped::End;
  }
  Stepped::Go
}

/// Answers the client's CRAM-MD5 response naming `user` with the key its token's record keeps
/// ([`crate::credentials::cram_md5_key`]), or none, answered as `svnserve` answers a user it has no password for; an
/// established session's principal is the token's.
async fn keyed(
  stream: &TcpStream,
  conversation: &mut Conversation,
  user: &[u8],
  out: &mut Vec<u8>,
) -> Stepped {
  out.clear();
  let found = crate::credentials::cram_md5_key(user).await;
  if found.is_err() {
    crate::fleet::count_refusal(UNKEYED);
  }
  let key = found.as_ref().ok().map(|found| &found.key);
  match conversation.connection.key(key, out) {
    Ok(Verdict::Established(_)) => {
      conversation.principal = found.ok().map(|found| found.principal);
    }
    Ok(_) => {}
    Err(_) => return Stepped::End,
  }
  if !out.is_empty() && stream.write_all(out).await.is_err() {
    return Stepped::End;
  }
  Stepped::Go
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
  /// A Subversion repository: its id and its store's UUID.
  Repository(RepositoryId, Uuid),
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
      Some(uuid) => Held::Repository(id, uuid),
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
  conversation: &mut Conversation,
  found: &Found,
  located: Option<&Located>,
  out: &mut Vec<u8>,
) -> Stepped {
  out.clear();
  let connection = &mut conversation.connection;
  let go = match located.map(|located| (located, held(&located.name))) {
    Some((located, Held::Repository(id, uuid))) => {
      let asked = open(connection, found, located.depth, uuid, out);
      if asked {
        conversation.opened = Some(Opened {
          id,
          name: located.name.clone(),
        });
      }
      asked
    }
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
  let stepped = opened(&stream, &mut conversation, &found, Some(&located), &mut out).await;
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
pub(crate) mod tests;
