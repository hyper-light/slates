//! The store's owner: one thread a node runs that holds its [`Store`], and the handle every shard reaches it through
//! (A-767 item 4; the wiring agreed with w-hg, w-git and w-svn).
//!
//! **Why a thread.** A landing ends with the log's flush (`F_FULLFSYNC` on macOS: milliseconds), and the engine reads
//! its pages with the device's waits; neither may stall a shard, which never blocks (§4.3). So the store lives on a
//! thread of its own, which the daemon spawns ([`StoreOwner::start`]) and joins ([`StoreOwner::stop`]); a node has one,
//! since its device has one log (A-767 item 2: concurrent landings share the flush).
//!
//! **How shards reach it.** A [`StoreHandle`], cloned to every shard that serves a repository, sends a request over a
//! bounded channel (`slates_rt::sync`, no `unsafe` and no `Arc`) and waits for its answer on a one-value channel of its
//! own, as a task: the shard serves other work meanwhile. A landing's sections travel as the `Vec`s the tool already
//! holds, moved into the request, so the owner writes them without a copy. The channel's capacity is the requests the
//! daemon lets wait at once; past it a request is refused `Busy`, the push's own refusal.
//!
//! **What an ask waits on** (goal 93, main's order of 2026-11-25: hg's 57k clone waited 121.35 s on 6,232 store asks,
//! the bulk of the clone). Every request carries when its asker sent it, on the host's one monotonic clock
//! (`slates_machine::clock::monotonic_ns`), and the owner sends back how it served it ([`Served`]) before its answer:
//! queued behind other work the owner was serving, or the owner's own wake from waiting for work; its service, and the
//! changeset log's reads within it. The asking task adds the hop back, so each ask's wait splits into parts that sum to
//! it to the nanosecond. A fetch's reads (texts, a text's slice, rows by tool, places) are summed on the asking thread
//! ([`take_read_waits`]), which the server counts after each want. Measured in-process (debug, a 2,000-changeset clone,
//! this MacBook): 4,015 requests, 1.42 ms each, 85.5% of it the owner's wake from its blocking receive, 12.2% its
//! service, 1.8% the hop back, 0.4% its log reads.
//!
//! **Spinning before it parks** (main's ruling of 2026-11-25: hg's 57k clone, 812,896 requests, waited 144.82 s of
//! 167.27 s on the owner's wake, about 178 µs a request). A fetch asks again as soon as its shard has taken the last
//! answer, so the owner, waiting for work, spins through the gap before it parks: for the gap it has learned (RFC 6298's
//! smoothed mean and four mean deviations, [`Gaps`]), at most the machine's measured wake (`spin_before_park_ns`, which
//! the daemon passes), so it spins only while asks arrive back to back. Counted, a request at a time: met spinning,
//! met parked, and the nanoseconds spun ([`AskWaits`]).
//!
//! **A sliced landing** ([`StoreHandle::begin`], then [`SlicedLanding`]'s `section`, `part` and `commit`) lands a
//! changeset too large to hold whole (an import, a large pack) one bounded part a request: each part is written where
//! it belongs in the open record and let go, so the RAM it takes is a part, never the changeset; the record is visible
//! only at its commit mark, as a whole landing's is. The owner serves reads meanwhile; another landing, whole or sliced,
//! waits in the owner's own bounded queue (the request channel's capacity again, refused `Busy` past it) until the open
//! one commits or is let go. A client that lets its [`SlicedLanding`] go uncommitted, or whose task is dropped,
//! abandons it: the owner learns so from the landing's liveness channel, whose sender the client held, at the next
//! request it serves (the drop sends one to wake it), and the log's end never moved, so the next record is written over
//! the abandoned bytes and recovery never sees them (a record verifies only with its trailer).
//!
//! **Ending.** The daemon stops the owner ([`StoreOwner::stop`]): the owner ends at the stop, lets its open landing go,
//! checkpoints the engine at the log's end (so a restart replays nothing) and its thread returns, and the stop joins it.
//! A checkpoint that fails is logged. Every request queued or sent after the stop is refused `Gone`: the queue closes
//! with the owner, and the requests in it drop with their answers. Before (main 01a20182's ruling of 2026-11-23), the
//! stop let only the daemon's own handle go and joined an owner that ended once every handle was gone, so a handle a
//! task still held made it a blind join: the fleet's harness waited in it 75 minutes. An owner whose every handle goes
//! without a stop ends the same way.
//!
//! **A store that cannot open (goal 93; jupiter's red of 2026-11-21).** The owner never ends with no word: it logs the
//! store's path and the open's reason once, and answers every request with [`StoreError::Unopened`] naming it until the
//! daemon lets it go, so a repository's creation answers the store's own reason (the server's `StoreRefused`). Before,
//! its thread ended at once, every request met a closed queue (`Gone`), and creations answered `Overloaded`.

use std::collections::VecDeque;
use std::fmt;

use slates_land::store_root::StoreRoot;
use slates_rt::sync::{self, ChannelReceiver, Sender, SyncError};

use crate::log::Section;
use crate::log::{Header, Located, SectionKind};
use crate::staging::{Identity, Staging};
use crate::store::{Changeset, Contents, PIECE_BYTES, Sizing, Store, StoreError};
use crate::texts::{Place, TextCache, TextRow};

/// A landed changeset's section as a replay reads it: its kind, where its bytes are in the log and their length, and
/// the bytes themselves when they fit one read piece ([`PIECE_BYTES`]: a record, a small `Texts`); a longer section is
/// read by its place a piece at a time ([`StoreHandle::read`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionAt {
  /// The section.
  pub section: Section,
  /// Its bytes, when they fit one piece.
  pub inline: Option<Vec<u8>>,
}

/// A section of a landing: its kind and its bytes, moved in.
pub type Part = (SectionKind, Vec<u8>);

/// The rows a texts-by-tool ask answers: for each log asked, each tool id's text identity and row, none where it has
/// none.
type TextsAnswer = Result<Vec<Vec<Option<(Identity, TextRow)>>>, StoreError>;

/// A request to the owner, with the channel its answer goes back on.
enum Request {
  Begin {
    header: Header,
    alive: ChannelReceiver<()>,
    answer: Sender<Result<u64, StoreError>>,
  },
  Section {
    landing: u64,
    kind: SectionKind,
    len: u64,
    answer: Sender<Result<(), StoreError>>,
  },
  /// A part's bytes, answered with the same buffer emptied once written, for the landing's next part
  /// ([`SlicedLanding::part`]).
  Part {
    landing: u64,
    bytes: Vec<u8>,
    answer: Sender<Result<Vec<u8>, StoreError>>,
  },
  Commit {
    landing: u64,
    answer: Sender<Result<Located, StoreError>>,
  },
  /// The daemon stops the owner ([`StoreOwner::stop`]): it ends now, whatever handles are still held.
  Stop,
  /// A sliced landing was let go: wakes the owner to look at its liveness channel.
  Dropped,
  Land {
    header: Header,
    parts: Vec<Part>,
    answer: Sender<Result<Located, StoreError>>,
  },
  Piece {
    identity: Identity,
    len: u64,
    offset: u64,
    answer: Sender<Result<Option<Vec<u8>>, StoreError>>,
  },
  Sections {
    row: Changeset,
    answer: Sender<Result<Vec<SectionAt>, StoreError>>,
  },
  Read {
    offset: u64,
    len: u64,
    answer: Sender<Result<Vec<u8>, StoreError>>,
  },
  Changesets {
    repository: u128,
    from: u64,
    limit: usize,
    answer: Sender<Result<Vec<Changeset>, StoreError>>,
  },
  Text {
    repository: u128,
    identity: Identity,
    offset: u64,
    /// The most bytes the answer copies: a piece ([`PIECE_BYTES`]) or a caller's step ([`StoreHandle::text_slice`]).
    most: u64,
    answer: Sender<Result<Option<Vec<u8>>, StoreError>>,
  },
  Texts {
    repository: u128,
    identities: Vec<Identity>,
    most: u64,
    answer: Sender<Result<MadeTexts, StoreError>>,
  },
  TextRow {
    repository: u128,
    identity: Identity,
    answer: Sender<Result<Option<TextRow>, StoreError>>,
  },
  Forget {
    repository: u128,
    sequence: u64,
    answer: Sender<Result<u64, StoreError>>,
  },
  PathTexts {
    repository: u128,
    path: Vec<u8>,
    from: u64,
    limit: usize,
    answer: Sender<Result<Vec<(u64, Identity)>, StoreError>>,
  },
  TextsByTool {
    repository: u128,
    logs: Vec<(Vec<u8>, Vec<Vec<u8>>)>,
    answer: Sender<TextsAnswer>,
  },
  Places {
    places: Vec<Place>,
    answer: Sender<Result<Vec<Vec<u8>>, StoreError>>,
  },
}

/// A request as the owner receives it: when its asker sent it ([`slates_machine::clock::monotonic_ns`], one clock for
/// every thread of the host) and where its service's timing goes back ([`Served`]), none for the daemon's own stop.
struct Asked {
  sent_ns: u64,
  timed: Option<Sender<Served>>,
  request: Request,
}

impl fmt::Debug for Asked {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Asked")
      .field("sent_ns", &self.sent_ns)
      .field("request", &self.request)
      .finish_non_exhaustive()
  }
}

impl Asked {
  /// `request` with no asker waiting on its timing (the daemon's stop, a dropped landing's wake).
  fn untimed(request: Request) -> Self {
    Asked {
      sent_ns: 0,
      timed: None,
      request,
    }
  }
}

/// How the owner served one request (goal 93, main's order of 2026-11-25: hg's 57k clone waited 121.35 s on 6,232 store
/// asks, about 19 ms an ask, the bulk of its 165.8 s): sent back before its answer, so its asker splits its wait.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Served {
  /// Queued behind other requests the owner was serving when it was sent.
  behind_ns: u64,
  /// The owner's own wake: sent while the owner waited for work, until it took the request.
  wake_ns: u64,
  /// The owner's service, from taking it to answering (its disk reads among it).
  serve_ns: u64,
  /// The changeset log's reads within the service ([`crate::log::ChangesetLog::read_ns`]).
  disk_ns: u64,
  /// When it was answered.
  done_ns: u64,
  /// How the owner, waiting for work, met it: spinning (a hit), parked, or neither (it was queued).
  met: Met,
}

/// How the owner met a request while it waited for work (goal 93, main's ruling of 2026-11-25: the 57k clone's store
/// wait was 86.6% the owner's wake, 144.82 s of 167.27 s over 812,896 requests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Met {
  /// It was queued: the owner was not waiting.
  #[default]
  Queued,
  /// The owner spun for it, this long, and it came: no wake.
  Spun(u64),
  /// The owner spun this long, then parked, and woke for it.
  Parked(u64),
}

/// What a handle's asks of the owner waited on, all told since it was last taken ([`StoreHandle::take_waits`]): each
/// ask's wait split into its parts, which sum to its whole (goal 93, main's order of 2026-11-25).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AskWaits {
  /// The asks answered.
  pub asks: u64,
  /// Their whole waits, from the send to the answer's arrival on the asking task.
  pub total_ns: u64,
  /// Queued behind other requests the owner was serving.
  pub behind_ns: u64,
  /// The owner's wake from waiting for work.
  pub wake_ns: u64,
  /// The owner's service but its disk reads: making texts, reading the engine's rows, copying.
  pub serve_ns: u64,
  /// The changeset log's reads within the service.
  pub disk_ns: u64,
  /// From the owner's answer to the asking task's taking it: the hop back and the asking shard's own turn.
  pub back_ns: u64,
  /// The longest whole wait.
  pub most_ns: u64,
  /// The asks the owner met spinning, no wake ([`StoreOwner::start`]'s spin).
  pub spin_hits: u64,
  /// The asks the owner met parked, woken for them.
  pub parks: u64,
  /// The nanoseconds the owner spun waiting for them, hits and misses.
  pub spin_ns: u64,
}

impl AskWaits {
  /// One ask more: sent at `sent_ns`, served as `served` says, taken at `taken_ns`.
  fn add(&mut self, served: &Served, (sent_ns, taken_ns): (u64, u64)) {
    let total = taken_ns.saturating_sub(sent_ns);
    self.asks = self.asks.saturating_add(1);
    self.total_ns = self.total_ns.saturating_add(total);
    self.behind_ns = self.behind_ns.saturating_add(served.behind_ns);
    self.wake_ns = self.wake_ns.saturating_add(served.wake_ns);
    self.serve_ns = self
      .serve_ns
      .saturating_add(served.serve_ns.saturating_sub(served.disk_ns));
    self.disk_ns = self.disk_ns.saturating_add(served.disk_ns);
    self.back_ns = self
      .back_ns
      .saturating_add(taken_ns.saturating_sub(served.done_ns));
    self.most_ns = self.most_ns.max(total);
    match served.met {
      Met::Queued => {}
      Met::Spun(ns) => {
        self.spin_hits = self.spin_hits.saturating_add(1);
        self.spin_ns = self.spin_ns.saturating_add(ns);
      }
      Met::Parked(ns) => {
        self.parks = self.parks.saturating_add(1);
        self.spin_ns = self.spin_ns.saturating_add(ns);
      }
    }
  }
}

std::thread_local! {
  /// What this thread's reads of the owner waited on since last taken ([`take_read_waits`]): a fetch's asks (texts, a
  /// text's slice, rows by tool, places), which the asking shard's tasks make on its thread alone. The shard's state
  /// lives the same way (`slates-server`'s `state`); a task never moves between shards.
  static READ_WAITS: core::cell::Cell<AskWaits> = const { core::cell::Cell::new(AskWaits {
    asks: 0,
    total_ns: 0,
    behind_ns: 0,
    wake_ns: 0,
    serve_ns: 0,
    disk_ns: 0,
    back_ns: 0,
    most_ns: 0,
    spin_hits: 0,
    parks: 0,
    spin_ns: 0,
  }) };
}

/// What this thread's reads of the owner (a fetch's asks: texts, a text's slice, rows by tool, places) waited on since
/// last taken, each split into its parts ([`AskWaits`]); none left after (goal 93, main's order of 2026-11-25).
pub fn take_read_waits() -> AskWaits {
  READ_WAITS.with(core::cell::Cell::take)
}

std::thread_local! {
  /// The flush of the last changeset this thread's asks landed ([`StoreHandle::land`], [`SlicedLanding::commit`]):
  /// the store's own `sync_data` (`Located::flush_ns`), none since it was last taken.
  static LANDED_FLUSH_NS: core::cell::Cell<Option<u64>> = const { core::cell::Cell::new(None) };
}

/// The device's durable flush as the last changeset this thread's asks landed took it, the store's own `sync_data`
/// without the owner's queue or the hop back (goal 93 P7, main's ruling of 2026-11-25: the import's rent-or-buy bound
/// prices a batch against it); none since it was last taken.
pub fn take_landed_flush_ns() -> Option<u64> {
  LANDED_FLUSH_NS.with(core::cell::Cell::take)
}

/// Notes `located`'s flush as the last this thread's asks landed ([`take_landed_flush_ns`]).
fn landed_flush(located: &Located) {
  LANDED_FLUSH_NS.with(|flush| flush.set(Some(located.flush_ns)));
}

/// The owner's timing of the request it serves ([`Served`]), sent before its answer.
struct Timer {
  timed: Option<Sender<Served>>,
  behind_ns: u64,
  wake_ns: u64,
  dequeued_ns: u64,
  disk_before: u64,
  met: Met,
}

impl Timer {
  /// The timing of a request sent at `sent_ns`, taken at `dequeued_ns` by an owner that began waiting for work at
  /// `idle_ns`: sent after it, its wait was the owner's wake; before, it queued behind the owner's other work.
  fn of(
    asked: (u64, Option<Sender<Served>>),
    (idle_ns, dequeued_ns, met): (u64, u64, Met),
    store: &Store,
  ) -> Self {
    let (sent_ns, timed) = asked;
    let waited = dequeued_ns.saturating_sub(sent_ns);
    let (behind_ns, wake_ns, met) = if sent_ns >= idle_ns {
      (0, waited, met)
    } else {
      (waited, 0, Met::Queued)
    };
    Timer {
      timed,
      behind_ns,
      wake_ns,
      dequeued_ns,
      disk_before: store.log().read_ns(),
      met,
    }
  }

  /// Answers `value` on `answer`, its timing sent first so the asker finds it with the answer.
  fn reply<T>(&mut self, store: &Store, answer: &Sender<T>, value: T) {
    if let Some(timed) = self.timed.take() {
      let done_ns = slates_machine::clock::monotonic_ns();
      let _ = timed.try_send(Served {
        behind_ns: self.behind_ns,
        wake_ns: self.wake_ns,
        serve_ns: done_ns.saturating_sub(self.dequeued_ns),
        disk_ns: store.log().read_ns().saturating_sub(self.disk_before),
        done_ns,
        met: self.met,
      });
    }
    let _ = answer.try_send(value);
  }
}

/// Why a request to the owner did not get its answer.
#[derive(Debug)]
pub enum OwnerError {
  /// The owner's queue is full: the caller refuses or retries (the push's own refusal).
  Busy,
  /// The owner is gone (stopped, or its thread ended).
  Gone,
  /// The store refused.
  Store(StoreError),
  /// The handle was used where a task's wait is not (a plain thread uses the store directly, or a test drives a shard),
  /// or the owner's stop, a plain thread's wait, was asked on a shard, which never blocks.
  NotOnShard,
}

impl fmt::Display for OwnerError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Busy => f.write_str("the store's owner has no room for another request now"),
      Self::Gone => f.write_str("the store's owner is gone"),
      Self::Store(error) => write!(f, "{error}"),
      Self::NotOnShard => f.write_str("the store's handle waits only as a shard's task"),
    }
  }
}

impl std::error::Error for OwnerError {}

/// The owner's thread, held by the daemon.
#[derive(Debug)]
pub struct StoreOwner {
  thread: slates_rt::registry::WaitedThread<Result<(), StoreError>>,
  handle: StoreHandle,
}

/// A shard's way to the owner; clone one for each shard.
#[derive(Debug, Clone)]
pub struct StoreHandle {
  requests: Sender<Asked>,
  /// The id of the standing grant the store's directory was opened under ([`crate::store::grant_id`]): what a
  /// landing's header names.
  grant: u64,
}

impl fmt::Debug for Request {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Request::Begin { header, .. } => f
        .debug_struct("Begin")
        .field("header", header)
        .finish_non_exhaustive(),
      Request::Section {
        landing, kind, len, ..
      } => f
        .debug_struct("Section")
        .field("landing", landing)
        .field("kind", kind)
        .field("len", len)
        .finish_non_exhaustive(),
      Request::Part { landing, bytes, .. } => f
        .debug_struct("Part")
        .field("landing", landing)
        .field("len", &bytes.len())
        .finish_non_exhaustive(),
      Request::Commit { landing, .. } => f
        .debug_struct("Commit")
        .field("landing", landing)
        .finish_non_exhaustive(),
      Request::Stop => f.write_str("Stop"),
      Request::Dropped => f.write_str("Dropped"),
      Request::Land { header, parts, .. } => f
        .debug_struct("Land")
        .field("header", header)
        .field("parts", &parts.len())
        .finish_non_exhaustive(),
      Request::Piece { len, offset, .. } => f
        .debug_struct("Piece")
        .field("len", len)
        .field("offset", offset)
        .finish_non_exhaustive(),
      Request::Sections { row, .. } => f
        .debug_struct("Sections")
        .field("row", row)
        .finish_non_exhaustive(),
      Request::Text {
        repository, offset, ..
      } => f
        .debug_struct("Text")
        .field("repository", repository)
        .field("offset", offset)
        .finish_non_exhaustive(),
      Request::Forget {
        repository,
        sequence,
        ..
      } => f
        .debug_struct("Forget")
        .field("repository", repository)
        .field("sequence", sequence)
        .finish_non_exhaustive(),
      Request::Texts {
        repository,
        identities,
        most,
        ..
      } => f
        .debug_struct("Texts")
        .field("repository", repository)
        .field("identities", &identities.len())
        .field("most", most)
        .finish_non_exhaustive(),
      Request::TextRow { repository, .. } => f
        .debug_struct("TextRow")
        .field("repository", repository)
        .finish_non_exhaustive(),
      Request::PathTexts {
        repository, from, ..
      } => f
        .debug_struct("PathTexts")
        .field("repository", repository)
        .field("from", from)
        .finish_non_exhaustive(),
      Request::TextsByTool {
        repository, logs, ..
      } => f
        .debug_struct("TextsByTool")
        .field("repository", repository)
        .field("logs", &logs.len())
        .finish_non_exhaustive(),
      Request::Places { places, .. } => f
        .debug_struct("Places")
        .field("places", &places.len())
        .finish_non_exhaustive(),
      Request::Read { offset, len, .. } => f
        .debug_struct("Read")
        .field("offset", offset)
        .field("len", len)
        .finish_non_exhaustive(),
      Request::Changesets {
        repository, from, ..
      } => f
        .debug_struct("Changesets")
        .field("repository", repository)
        .field("from", from)
        .finish_non_exhaustive(),
    }
  }
}

impl StoreOwner {
  /// Opens the store in `root` on a thread of its own (the store recovers there before the first request is served),
  /// with room for `waiting` requests at once; waiting for work, it spins at most `spin_cap_ns` (the machine's
  /// `spin_before_park_ns`: a wake's measured cost) before it parks ([`Gaps`]).
  pub fn start(
    root: StoreRoot,
    sizing: Sizing,
    (waiting, spin_cap_ns): (usize, u64),
  ) -> Result<Self, OwnerError> {
    let grant = crate::store::grant_id(root.grant().target.device, root.grant().target.inode);
    let (requests, incoming) = sync::channel(waiting).map_err(|_| OwnerError::Busy)?;
    let thread = slates_rt::registry::spawn_waited(
      std::thread::Builder::new().name("slates-store".to_owned()),
      move || serve(root, sizing, incoming, (waiting, spin_cap_ns)),
    )
    .map_err(|_| OwnerError::Gone)?;
    Ok(Self {
      thread,
      handle: StoreHandle { requests, grant },
    })
  }

  /// A handle for a shard.
  pub fn handle(&self) -> StoreHandle {
    self.handle.clone()
  }

  /// Stops the owner and joins it, on a plain thread (the daemon's, once its shards have stopped): its open landing let
  /// go, its checkpoint written, its store closed. The owner ends at the stop whatever handles are still held, so the
  /// join waits for the request it is serving and the checkpoint, never for a handle to go; every request queued or
  /// sent after it is refused [`OwnerError::Gone`] (its queue closes with it). The owner's own failure (its open or its
  /// checkpoint) is the answer.
  ///
  /// # Errors
  /// `NotOnShard` when asked on a shard (it would block one); `Gone` when the owner's thread did not end cleanly; the
  /// store's refusal of its open or its checkpoint.
  pub fn stop(self) -> Result<(), OwnerError> {
    match self
      .handle
      .requests
      .blocking_send(Asked::untimed(Request::Stop))
    {
      // Sent, or the owner is gone already: either way the join ends.
      Ok(()) | Err(SyncError::Closed(_)) => {}
      Err(SyncError::NotOnShardThread(_) | SyncError::Full(_)) => {
        return Err(OwnerError::NotOnShard);
      }
    }
    drop(self.handle);
    match self.thread.join() {
      Ok(result) => result.map_err(OwnerError::Store),
      Err(_) => Err(OwnerError::Gone),
    }
  }
}

/// The gaps the owner waited for work, learned as RFC 6298 learns a round trip (its smoothed mean and mean deviation,
/// gains 1/8 and 1/4): from a request it met while waiting (spun for or parked for) to the request's taking. Its window
/// is a retransmission timeout's (the mean and four deviations), capped (goal 93, main's ruling of 2026-11-25: the 57k
/// clone's store wait was 86.6% the owner's wake, 178 µs a request; a fetch's next ask follows its answer by its shard's
/// turn, so while asks arrive back to back the owner spins through the gap, and once they stop it parks).
#[derive(Clone, Copy, Debug, Default)]
struct Gaps {
  /// The smoothed gap, none learned yet.
  mean_ns: Option<u64>,
  /// Its mean deviation.
  deviation_ns: u64,
}

/// Format: RFC 6298's gain of a new sample into the smoothed mean (alpha = 1/8), as a shift.
const MEAN_GAIN_SHIFT: u32 = 3;
/// Format: RFC 6298's gain of a new sample's deviation into the mean deviation (beta = 1/4), as a shift.
const DEVIATION_GAIN_SHIFT: u32 = 2;
/// Format: RFC 6298's weight of the deviation in a timeout (K = 4).
const DEVIATION_WEIGHT: u64 = 4;

impl Gaps {
  /// One gap more.
  fn learn(&mut self, gap_ns: u64) {
    match self.mean_ns {
      None => {
        self.mean_ns = Some(gap_ns);
        self.deviation_ns = gap_ns / 2;
      }
      Some(mean) => {
        let off = mean.abs_diff(gap_ns);
        self.deviation_ns = self
          .deviation_ns
          .saturating_sub(self.deviation_ns >> DEVIATION_GAIN_SHIFT)
          .saturating_add(off >> DEVIATION_GAIN_SHIFT);
        self.mean_ns = Some(
          mean
            .saturating_sub(mean >> MEAN_GAIN_SHIFT)
            .saturating_add(gap_ns >> MEAN_GAIN_SHIFT),
        );
      }
    }
  }

  /// How long to spin before parking: the mean and four deviations, at most `cap_ns`; none before a gap is learned.
  fn window(&self, cap_ns: u64) -> u64 {
    self.mean_ns.map_or(0, |mean| {
      mean
        .saturating_add(self.deviation_ns.saturating_mul(DEVIATION_WEIGHT))
        .min(cap_ns)
    })
  }
}

/// The next request, met spinning for at most `window_ns` and then parked for ([`Met`]); none once every sender is gone.
fn waited_for(incoming: &mut ChannelReceiver<Asked>, window_ns: u64) -> Option<(Asked, Met)> {
  let began = slates_machine::clock::monotonic_ns();
  let mut spun = 0u64;
  while spun < window_ns {
    match incoming.try_recv() {
      Ok(Some(asked)) => return Some((asked, Met::Spun(spun))),
      Ok(None) => core::hint::spin_loop(),
      Err(_) => return None,
    }
    spun = slates_machine::clock::monotonic_ns().saturating_sub(began);
  }
  incoming
    .blocking_recv()
    .ok()
    .map(|asked| (asked, Met::Parked(spun)))
}

/// The refusal an unopened store answers a request with.
fn unopened<T>(reason: &str) -> Result<T, StoreError> {
  Err(StoreError::Unopened(reason.into()))
}

/// Answers every request with [`StoreError::Unopened`] naming `reason`, until every handle is gone: the owner of a store
/// that could not be opened.
fn refuse_all(incoming: &mut ChannelReceiver<Asked>, reason: &str) {
  while let Ok(asked) = incoming.blocking_recv() {
    match asked.request {
      Request::Begin { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Section { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Part { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Commit { answer, .. } | Request::Land { answer, .. } => {
        drop(answer.try_send(unopened(reason)))
      }
      Request::Stop => return,
      Request::Dropped => {}
      Request::Piece { answer, .. } | Request::Text { answer, .. } => {
        drop(answer.try_send(unopened(reason)))
      }
      Request::Sections { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Read { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Changesets { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Texts { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::TextRow { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Forget { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::PathTexts { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::TextsByTool { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Places { answer, .. } => drop(answer.try_send(unopened(reason))),
    }
  }
}

/// The owner's loop: open (recovering), then answer requests until every handle is gone, then checkpoint.
fn serve(
  root: StoreRoot,
  sizing: Sizing,
  mut incoming: ChannelReceiver<Asked>,
  (waiting, spin_cap_ns): (usize, u64),
) -> Result<(), StoreError> {
  let mut gaps = Gaps::default();
  let at = root.target().key.clone();
  let mut store = match Store::open(root, sizing) {
    Ok(store) => store,
    Err(error) => {
      // Never ended with no word (goal 93, jupiter's red): the reason is logged once, and every request is answered with
      // it until the daemon lets the owner go, so a creation names it rather than a gone owner or a busy shard.
      let reason = error.to_string().into_boxed_str();
      slates_machine::log_line!(
        "slates-store: the version-control store at {at} could not be opened: {reason}; every request is refused with it"
      );
      refuse_all(&mut incoming, &reason);
      return Err(error);
    }
  };
  // The owner lands bytes the request carries; nothing is staged on its side.
  let staging = Staging::new(0);
  // The texts the owner made from their chains (A-645's derived per-path read form), kept across requests, so a clone
  // streaming a path's versions in order makes each from the one before: those the last `waiting` reads lent, one a
  // waiting request (`TextCache::for_reads`).
  let mut texts = TextCache::for_reads(waiting);
  let mut landings = Landings {
    open: None,
    held: VecDeque::new(),
    waiting,
    next: 0,
  };
  loop {
    landings.let_go_if_dropped(&mut store);
    // From here the owner waits for work: a request sent after it waited on the owner's wake, one before on its work.
    let idle_ns = slates_machine::clock::monotonic_ns();
    let (asked, met) = match landings.next_held() {
      Some(asked) => (asked, Met::Queued),
      None => match waited_for(&mut incoming, gaps.window(spin_cap_ns)) {
        Some(waited) => waited,
        None => break,
      },
    };
    let dequeued_ns = slates_machine::clock::monotonic_ns();
    if met != Met::Queued {
      gaps.learn(dequeued_ns.saturating_sub(idle_ns));
    }
    landings.let_go_if_dropped(&mut store);
    let Some(Asked {
      sent_ns,
      timed,
      request,
    }) = landings.hold_while_open(asked)
    else {
      continue;
    };
    let mut timer = Timer::of((sent_ns, timed), (idle_ns, dequeued_ns, met), &store);
    match request {
      Request::Begin {
        header,
        alive,
        answer,
      } => {
        let answered = landings.begin(&mut store, header, alive);
        timer.reply(&store, &answer, answered);
      }
      Request::Section {
        landing,
        kind,
        len,
        answer,
      } => {
        let step = landings
          .check(landing)
          .and_then(|()| store.landing_section(kind, len));
        let answered = landings.failed_lets_go(&mut store, step);
        timer.reply(&store, &answer, answered);
      }
      Request::Part {
        landing,
        mut bytes,
        answer,
      } => {
        let step = landings.check(landing).and_then(|()| {
          if bytes.len() > PIECE_BYTES {
            return Err(StoreError::Sizing("a landing's part past one piece"));
          }
          store.landing_part(&bytes)
        });
        // Written, the buffer goes back emptied for the landing's next part: one buffer a landing, not one a part.
        let given = landings.failed_lets_go(&mut store, step).map(|()| {
          bytes.clear();
          bytes
        });
        let answered = given;
        timer.reply(&store, &answer, answered);
      }
      Request::Commit { landing, answer } => {
        // A commit that fails lets the record go (`commit_open` took it); a stranger's commit touches nothing.
        let committed = landings.check(landing).and_then(|()| {
          landings.open = None;
          store.commit_landing()
        });
        let answered = committed;
        timer.reply(&store, &answer, answered);
      }
      // The daemon's stop: the loop ends here, and the open landing is let go and the checkpoint written below.
      Request::Stop => break,
      Request::Dropped => {}
      Request::Land {
        header,
        parts,
        answer,
      } => {
        let sections: Vec<(SectionKind, Contents<'_>)> = parts
          .iter()
          .map(|(kind, bytes)| (*kind, Contents::Bytes(bytes)))
          .collect();
        let answered = store.land(header, &sections, &staging);
        timer.reply(&store, &answer, answered);
      }
      Request::Piece {
        identity,
        len,
        offset,
        answer,
      } => {
        let read = store
          .piece(&staging, &identity, len, offset)
          .map(|piece| piece.map(std::borrow::Cow::into_owned));
        let answered = read;
        timer.reply(&store, &answer, answered);
      }
      Request::Changesets {
        repository,
        from,
        limit,
        answer,
      } => {
        let answered = store.changesets(repository, from, limit);
        timer.reply(&store, &answer, answered);
      }
      Request::Sections { row, answer } => {
        let answered = sections_of(&store, &row);
        timer.reply(&store, &answer, answered);
      }
      Request::Text {
        repository,
        identity,
        offset,
        most,
        answer,
      } => {
        // As far as `most` reaches from `offset` (a piece, as `piece` answers, or a caller's step): a caller walks
        // slices of any length.
        let read = store
          .text_slice(&staging, repository, (&identity, offset, most), &mut texts)
          .map(|text| text.map(<[u8]>::to_vec));
        let answered = read;
        timer.reply(&store, &answer, answered);
      }
      Request::Texts {
        repository,
        identities,
        most,
        answer,
      } => {
        let answered = texts_made(
          (&mut store, &staging, &mut texts),
          repository,
          (&identities, most),
        );
        timer.reply(&store, &answer, answered);
      }
      Request::TextRow {
        repository,
        identity,
        answer,
      } => {
        let answered = store.text_row(repository, &identity);
        timer.reply(&store, &answer, answered);
      }
      Request::Forget {
        repository,
        sequence,
        answer,
      } => {
        let answered = store.forget(repository, sequence);
        timer.reply(&store, &answer, answered);
      }
      Request::PathTexts {
        repository,
        path,
        from,
        limit,
        answer,
      } => {
        let answered = store.path_texts(repository, &path, from, limit);
        timer.reply(&store, &answer, answered);
      }
      Request::TextsByTool {
        repository,
        logs,
        answer,
      } => {
        let found: Result<Vec<Vec<_>>, StoreError> = logs
          .iter()
          .map(|(path, tool_ids)| store.texts_by_tools(repository, path, tool_ids))
          .collect();
        let answered = found;
        timer.reply(&store, &answer, answered);
      }
      Request::Places { places, answer } => {
        let answered = store.places_bytes(&staging, &places);
        timer.reply(&store, &answer, answered);
      }
      Request::Read {
        offset,
        len,
        answer,
      } => {
        let answered = store.read(offset, len);
        timer.reply(&store, &answer, answered);
      }
    }
  }
  store.abandon_landing();
  let checkpointed = store.checkpoint();
  if let Err(error) = &checkpointed {
    slates_machine::log_line!(
      "slates-store: the version-control store at {at} ended without its checkpoint: {error}; a restart replays its log"
    );
  }
  checkpointed
}

/// The owner's sliced landing, if one is open, and the landings held behind it.
struct Landings {
  /// The open landing: its number and the liveness channel whose sender its client holds.
  open: Option<(u64, ChannelReceiver<()>)>,
  /// Landings (whole or sliced) that arrived while one was open, in order.
  held: VecDeque<Asked>,
  /// The most held at once: the request channel's own capacity, so the owner's queue is bounded as the channel is.
  waiting: usize,
  /// The next landing's number.
  next: u64,
}

impl Landings {
  /// Lets the open landing go when its client has let its liveness sender go.
  fn let_go_if_dropped(&mut self, store: &mut Store) {
    let dropped = match &mut self.open {
      Some((_, alive)) => matches!(alive.try_recv(), Err(SyncError::Closed(()))),
      None => false,
    };
    if dropped {
      self.open = None;
      store.abandon_landing();
    }
  }

  /// The oldest held landing, once none is open.
  fn next_held(&mut self) -> Option<Asked> {
    if self.open.is_some() {
      return None;
    }
    self.held.pop_front()
  }

  /// `request`, to serve now; or `None` when it is a landing that must wait for the open one: held, or, when the
  /// queue is full, answered [`StoreError::Held`] (its asker's [`OwnerError::Busy`]).
  fn hold_while_open(&mut self, asked: Asked) -> Option<Asked> {
    if self.open.is_none() || !matches!(asked.request, Request::Land { .. } | Request::Begin { .. })
    {
      return Some(asked);
    }
    if self.held.len() < self.waiting {
      self.held.push_back(asked);
      return None;
    }
    match asked.request {
      Request::Land { answer, .. } => {
        let _ = answer.try_send(Err(StoreError::Held));
      }
      Request::Begin { answer, .. } => {
        let _ = answer.try_send(Err(StoreError::Held));
      }
      _ => {}
    }
    None
  }

  /// Opens a sliced landing; its number.
  fn begin(
    &mut self,
    store: &mut Store,
    header: Header,
    alive: ChannelReceiver<()>,
  ) -> Result<u64, StoreError> {
    store.open_landing(header)?;
    let number = self.next;
    self.next = self.next.wrapping_add(1);
    self.open = Some((number, alive));
    Ok(number)
  }

  /// Whether `landing` is the open one.
  fn check(&self, landing: u64) -> Result<(), StoreError> {
    match &self.open {
      Some((number, _)) if *number == landing => Ok(()),
      _ => Err(StoreError::NoLanding),
    }
  }

  /// `step`, letting the open landing go when it failed on it (a section's bytes out of order, a write refused), so a
  /// landing is committed whole or not at all.
  fn failed_lets_go(
    &mut self,
    store: &mut Store,
    step: Result<(), StoreError>,
  ) -> Result<(), StoreError> {
    if let Err(error) = &step
      && !matches!(error, StoreError::NoLanding)
    {
      self.open = None;
      store.abandon_landing();
    }
    step
  }
}

/// `row`'s sections, each small one with its bytes.
fn sections_of(store: &Store, row: &Changeset) -> Result<Vec<SectionAt>, StoreError> {
  let piece = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  store
    .sections(row)?
    .into_iter()
    .map(|section| {
      let inline = if section.len <= piece {
        Some(store.read(section.offset, section.len)?)
      } else {
        None
      };
      Ok(SectionAt { section, inline })
    })
    .collect()
}

impl StoreHandle {
  /// The id of the standing grant the store writes under: the `grant` every landing's header names (its `G` row).
  pub fn grant(&self) -> u64 {
    self.grant
  }

  /// Sends `request` and waits for its answer on `answers`, as a task: a fetch's read's wait split into its parts and
  /// added to this thread's ([`take_read_waits`]).
  async fn ask<T>(
    &self,
    request: Request,
    mut answers: ChannelReceiver<Result<T, StoreError>>,
  ) -> Result<T, OwnerError> {
    let read = matches!(
      request,
      Request::Texts { .. }
        | Request::Text { .. }
        | Request::TextsByTool { .. }
        | Request::Places { .. }
    );
    let (timed, mut timings) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let sent_ns = slates_machine::clock::monotonic_ns();
    let asked = Asked {
      sent_ns,
      timed: Some(timed),
      request,
    };
    match self.requests.try_send(asked) {
      Ok(()) => {}
      Err(SyncError::Full(_)) => return Err(OwnerError::Busy),
      Err(SyncError::Closed(_)) => return Err(OwnerError::Gone),
      Err(SyncError::NotOnShardThread(_)) => return Err(OwnerError::NotOnShard),
    }
    let answer = answers.recv().await;
    let taken_ns = slates_machine::clock::monotonic_ns();
    // Sent before the answer, so it is here whenever the answer is.
    if read && let Ok(Some(served)) = timings.try_recv() {
      READ_WAITS.with(|waits| {
        let mut sum = waits.get();
        sum.add(&served, (sent_ns, taken_ns));
        waits.set(sum);
      });
    }
    match answer {
      Ok(Err(StoreError::Held)) => Err(OwnerError::Busy),
      Ok(answer) => answer.map_err(OwnerError::Store),
      Err(SyncError::NotOnShardThread(())) => Err(OwnerError::NotOnShard),
      Err(_) => Err(OwnerError::Gone),
    }
  }

  /// Opens a sliced landing of `header` (whose section count names the sections to come); answered once its header is
  /// written. Its sections and parts follow through the [`SlicedLanding`]; nothing is visible until its commit.
  pub async fn begin(&self, header: Header) -> Result<SlicedLanding, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let (alive, watched) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let number = self
      .ask(
        Request::Begin {
          header,
          alive: watched,
          answer,
        },
        answers,
      )
      .await?;
    Ok(SlicedLanding {
      handle: self.clone(),
      number,
      alive: Some(alive),
    })
  }

  /// Lands a changeset whose sections are `parts` (their bytes moved in): appended, committed with one flush, its rows
  /// applied; answered once it is durable.
  pub async fn land(&self, header: Header, parts: Vec<Part>) -> Result<Located, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let landed = self
      .ask(
        Request::Land {
          header,
          parts,
          answer,
        },
        answers,
      )
      .await;
    if let Ok(located) = &landed {
      landed_flush(located);
    }
    landed
  }

  /// The landed content `identity` (of `len` bytes) from `offset`, as far as one piece reaches ([`Store::piece`]).
  pub async fn piece(
    &self,
    identity: Identity,
    len: u64,
    offset: u64,
  ) -> Result<Option<Vec<u8>>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Piece {
          identity,
          len,
          offset,
          answer,
        },
        answers,
      )
      .await
  }

  /// The sections of the landed changeset `row`, in order, each small one with its bytes ([`SectionAt`]): what a
  /// replay reads a changeset back by.
  pub async fn sections(&self, row: Changeset) -> Result<Vec<SectionAt>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self.ask(Request::Sections { row, answer }, answers).await
  }

  /// At most [`PIECE_BYTES`] of a section from `offset` (its place, as [`SectionAt`] gives it), as far as `len` reaches:
  /// a long section read back a piece at a time.
  pub async fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Read {
          offset,
          len,
          answer,
        },
        answers,
      )
      .await
  }

  /// The texts `identities` of `repository`, each made whole from its chain on the owner, in order, in one request
  /// (goal 93: a clone after P5 made each let-go changelog and manifest text by a request of its own, a piece a request,
  /// over 100,000 trips to the owner for hg's 57k history; main's interop bench, its clone about 1.7 times its load's
  /// slowing past stock's): [`MadeTexts::Past`] where they hold more than `most` bytes together, none made past it;
  /// [`MadeTexts::Missing`] where one is no text a landed changeset kept.
  pub async fn texts(
    &self,
    repository: u128,
    identities: Vec<Identity>,
    most: u64,
  ) -> Result<MadeTexts, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Texts {
          repository,
          identities,
          most,
          answer,
        },
        answers,
      )
      .await
  }

  /// The text `identity` of `repository` from `offset`, as far as one piece reaches, made from its chain on the owner
  /// (A-645's derived per-path read form, [`Store::text_at`]): empty at its end, none past it or for a text no landed
  /// changeset kept.
  pub async fn text(
    &self,
    repository: u128,
    identity: Identity,
    offset: u64,
  ) -> Result<Option<Vec<u8>>, OwnerError> {
    let piece = u64::try_from(PIECE_BYTES).unwrap_or(u64::MAX);
    self.text_slice(repository, identity, (offset, piece)).await
  }

  /// At most `most` bytes of the text `identity` of `repository` from `offset`, made from its chain on the owner
  /// ([`Store::text_slice`]): a fetch's text past its step's copy budget copied in slices of that budget, one a request
  /// (goal 93, main's ruling of 2026-11-25). Empty at its end, none past it or for a text no landed changeset kept.
  pub async fn text_slice(
    &self,
    repository: u128,
    identity: Identity,
    (offset, most): (u64, u64),
  ) -> Result<Option<Vec<u8>>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Text {
          repository,
          identity,
          offset,
          most,
          answer,
        },
        answers,
      )
      .await
  }

  /// How the text `identity` of `repository` is made ([`Store::text_row`]): its changeset, length, base and place, so a
  /// clone sends an edit as it lies (read with [`StoreHandle::piece`]).
  pub async fn text_row(
    &self,
    repository: u128,
    identity: Identity,
  ) -> Result<Option<TextRow>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::TextRow {
          repository,
          identity,
          answer,
        },
        answers,
      )
      .await
  }

  /// For each log of `logs` (a path and the versions its tool names, Mercurial's nodes), each version in `repository`
  /// as it landed ([`Store::texts_by_tools`]), in order, none for one no landed changeset named. One batch for many
  /// logs, so a stream asks by its room, never once a log (r20 on hg-src: about 12,000 asks, the serving shard idle half
  /// the stream); bounded by what the asker's room holds.
  pub async fn texts_by_tool(
    &self,
    repository: u128,
    logs: Vec<(Vec<u8>, Vec<Vec<u8>>)>,
  ) -> Result<Vec<Vec<Option<(Identity, TextRow)>>>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::TextsByTool {
          repository,
          logs,
          answer,
        },
        answers,
      )
      .await
  }

  /// The bytes at each of `places` ([`Store::place_bytes`]), in order: a stream's chunks, read in place.
  pub async fn places(&self, places: Vec<Place>) -> Result<Vec<Vec<u8>>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self.ask(Request::Places { places, answer }, answers).await
  }

  /// A path's history in `repository` from sequence `from` on, at most `limit` ([`Store::path_texts`]).
  pub async fn path_texts(
    &self,
    repository: u128,
    path: Vec<u8>,
    from: u64,
    limit: usize,
  ) -> Result<Vec<(u64, Identity)>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::PathTexts {
          repository,
          path,
          from,
          limit,
          answer,
        },
        answers,
      )
      .await
  }

  /// Forgets repository `repository`'s changeset `sequence`, one no register named ([`Store::forget`]): the rows forgotten
  /// or moved, none where no such changeset landed.
  pub async fn forget(&self, repository: u128, sequence: u64) -> Result<u64, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Forget {
          repository,
          sequence,
          answer,
        },
        answers,
      )
      .await
  }

  /// The repository's landed changesets from `from` on, at most `limit` ([`Store::changesets`]).
  pub async fn changesets(
    &self,
    repository: u128,
    from: u64,
    limit: usize,
  ) -> Result<Vec<Changeset>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Changesets {
          repository,
          from,
          limit,
          answer,
        },
        answers,
      )
      .await
  }
}

/// A landing open on the owner, its steps sent one bounded part at a time (the module's "A sliced landing"). Let go
/// uncommitted, it is abandoned: nothing of it is ever visible.
#[derive(Debug)]
pub struct SlicedLanding {
  handle: StoreHandle,
  number: u64,
  /// The liveness sender the owner watches; taken at the commit, so only an uncommitted landing's drop wakes the owner.
  alive: Option<Sender<()>>,
}

impl SlicedLanding {
  /// Begins the next section, `len` bytes of `kind`.
  pub async fn section(&mut self, kind: SectionKind, len: u64) -> Result<(), OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let request = Request::Section {
      landing: self.number,
      kind,
      len,
      answer,
    };
    self.handle.ask(request, answers).await
  }

  /// Writes the next `bytes` of the open section, at most one piece ([`PIECE_BYTES`]: the read's unit, so a landing in
  /// flight holds no more than a piece a request); moved in, and answered with the same buffer emptied once written, for
  /// the caller to fill with the next part (P7's finding (2): a fresh buffer a part was one of an import's copies of its
  /// every text, churn the system allocator keeps). Refused, the buffer is let go.
  pub async fn part(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let request = Request::Part {
      landing: self.number,
      bytes,
      answer,
    };
    self.handle.ask(request, answers).await
  }

  /// Commits the landing (one flush) and applies its rows; answered once it is durable.
  pub async fn commit(mut self) -> Result<Located, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    let request = Request::Commit {
      landing: self.number,
      answer,
    };
    let committed = self.handle.ask(request, answers).await;
    // Committed or refused, the owner let the record go: the drop has nothing to wake it for.
    self.alive = None;
    if let Ok(located) = &committed {
      landed_flush(located);
    }
    committed
  }
}

impl Drop for SlicedLanding {
  fn drop(&mut self) {
    if let Some(alive) = self.alive.take() {
      drop(alive);
      // Wakes the owner to see the landing let go. Refused only when its queue is full, and then it has requests to
      // serve, and looks at the landing before each.
      let _ = self
        .handle
        .requests
        .try_send(Asked::untimed(Request::Dropped));
    }
  }
}

/// What a [`StoreHandle::texts`] request made: every text asked, whole, in order; or why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MadeTexts {
  /// Each text, in the order asked.
  Made(Vec<Vec<u8>>),
  /// The texts hold more than the bytes allowed: none made past them.
  Past,
  /// A text no landed changeset kept.
  Missing,
}

/// Makes `identities` of `repository` whole, in order, within `most` bytes together ([`StoreHandle::texts`]).
fn texts_made(
  (store, staging, cache): (&mut Store, &Staging, &mut TextCache),
  repository: u128,
  (identities, most): (&[Identity], u64),
) -> Result<MadeTexts, StoreError> {
  let mut made = Vec::new();
  if made.try_reserve_exact(identities.len()).is_err() {
    return Ok(MadeTexts::Past);
  }
  let mut total = 0u64;
  for identity in identities {
    let Some(text) = store.text_whole(staging, repository, identity, cache)? else {
      return Ok(MadeTexts::Missing);
    };
    total = total.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    if total > most {
      return Ok(MadeTexts::Past);
    }
    made.push(text.to_vec());
  }
  Ok(MadeTexts::Made(made))
}
