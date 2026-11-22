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
//! **Ending.** When every handle is gone the owner checkpoints the engine at the log's end (so a restart replays
//! nothing) and its thread returns; [`StoreOwner::stop`] joins it. A checkpoint that fails is logged.
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
    answer: Sender<Result<Option<Vec<u8>>, StoreError>>,
  },
  TextRow {
    repository: u128,
    identity: Identity,
    answer: Sender<Result<Option<TextRow>, StoreError>>,
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

/// Why a request to the owner did not get its answer.
#[derive(Debug)]
pub enum OwnerError {
  /// The owner's queue is full: the caller refuses or retries (the push's own refusal).
  Busy,
  /// The owner is gone (stopped, or its thread ended).
  Gone,
  /// The store refused.
  Store(StoreError),
  /// The handle was used where a task's wait is not (a plain thread uses the store directly, or a test drives a shard).
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
  requests: Sender<Request>,
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
  /// with room for `waiting` requests at once.
  pub fn start(root: StoreRoot, sizing: Sizing, waiting: usize) -> Result<Self, OwnerError> {
    let grant = crate::store::grant_id(root.grant().target.device, root.grant().target.inode);
    let (requests, incoming) = sync::channel(waiting).map_err(|_| OwnerError::Busy)?;
    let thread = slates_rt::registry::spawn_waited(
      std::thread::Builder::new().name("slates-store".to_owned()),
      move || serve(root, sizing, incoming, waiting),
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

  /// Lets the daemon's own handle go and joins the owner once every other handle is gone: its checkpoint written, its
  /// store closed. The owner's own failure (its open or its checkpoint) is the answer.
  pub fn stop(self) -> Result<(), OwnerError> {
    drop(self.handle);
    match self.thread.join() {
      Ok(result) => result.map_err(OwnerError::Store),
      Err(_) => Err(OwnerError::Gone),
    }
  }
}

/// The refusal an unopened store answers a request with.
fn unopened<T>(reason: &str) -> Result<T, StoreError> {
  Err(StoreError::Unopened(reason.into()))
}

/// Answers every request with [`StoreError::Unopened`] naming `reason`, until every handle is gone: the owner of a store
/// that could not be opened.
fn refuse_all(incoming: &mut ChannelReceiver<Request>, reason: &str) {
  while let Ok(request) = incoming.blocking_recv() {
    match request {
      Request::Begin { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Section { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Part { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Commit { answer, .. } | Request::Land { answer, .. } => {
        drop(answer.try_send(unopened(reason)))
      }
      Request::Dropped => {}
      Request::Piece { answer, .. } | Request::Text { answer, .. } => {
        drop(answer.try_send(unopened(reason)))
      }
      Request::Sections { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Read { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::Changesets { answer, .. } => drop(answer.try_send(unopened(reason))),
      Request::TextRow { answer, .. } => drop(answer.try_send(unopened(reason))),
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
  mut incoming: ChannelReceiver<Request>,
  waiting: usize,
) -> Result<(), StoreError> {
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
  // streaming a path's versions in order makes each from the one before; its bound learned from the chains it made.
  let mut texts = TextCache::new(0);
  let mut landings = Landings {
    open: None,
    held: VecDeque::new(),
    waiting,
    next: 0,
  };
  loop {
    landings.let_go_if_dropped(&mut store);
    let request = match landings.next_held() {
      Some(request) => request,
      None => match incoming.blocking_recv() {
        Ok(request) => request,
        Err(_) => break,
      },
    };
    landings.let_go_if_dropped(&mut store);
    let Some(request) = landings.hold_while_open(request) else {
      continue;
    };
    match request {
      Request::Begin {
        header,
        alive,
        answer,
      } => {
        let _ = answer.try_send(landings.begin(&mut store, header, alive));
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
        let _ = answer.try_send(landings.failed_lets_go(&mut store, step));
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
        let _ = answer.try_send(given);
      }
      Request::Commit { landing, answer } => {
        // A commit that fails lets the record go (`commit_open` took it); a stranger's commit touches nothing.
        let committed = landings.check(landing).and_then(|()| {
          landings.open = None;
          store.commit_landing()
        });
        let _ = answer.try_send(committed);
      }
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
        let _ = answer.try_send(store.land(header, &sections, &staging));
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
        let _ = answer.try_send(read);
      }
      Request::Changesets {
        repository,
        from,
        limit,
        answer,
      } => {
        let _ = answer.try_send(store.changesets(repository, from, limit));
      }
      Request::Sections { row, answer } => {
        let _ = answer.try_send(sections_of(&store, &row));
      }
      Request::Text {
        repository,
        identity,
        offset,
        answer,
      } => {
        // As far as one piece reaches from `offset`, as `piece` answers: a caller walks pieces of any length.
        let read = store
          .text_at(&staging, repository, &identity, offset, &mut texts)
          .map(|text| {
            text.map(|rest| {
              rest
                .get(..rest.len().min(PIECE_BYTES))
                .unwrap_or(rest)
                .to_vec()
            })
          });
        let _ = answer.try_send(read);
      }
      Request::TextRow {
        repository,
        identity,
        answer,
      } => {
        let _ = answer.try_send(store.text_row(repository, &identity));
      }
      Request::PathTexts {
        repository,
        path,
        from,
        limit,
        answer,
      } => {
        let _ = answer.try_send(store.path_texts(repository, &path, from, limit));
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
        let _ = answer.try_send(found);
      }
      Request::Places { places, answer } => {
        let _ = answer.try_send(store.places_bytes(&staging, &places));
      }
      Request::Read {
        offset,
        len,
        answer,
      } => {
        let _ = answer.try_send(store.read(offset, len));
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
  held: VecDeque<Request>,
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
  fn next_held(&mut self) -> Option<Request> {
    if self.open.is_some() {
      return None;
    }
    self.held.pop_front()
  }

  /// `request`, to serve now; or `None` when it is a landing that must wait for the open one: held, or, when the
  /// queue is full, answered [`StoreError::Held`] (its asker's [`OwnerError::Busy`]).
  fn hold_while_open(&mut self, request: Request) -> Option<Request> {
    if self.open.is_none() || !matches!(request, Request::Land { .. } | Request::Begin { .. }) {
      return Some(request);
    }
    if self.held.len() < self.waiting {
      self.held.push_back(request);
      return None;
    }
    match request {
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

  /// Sends `request` and waits for its answer on `answers`, as a task.
  async fn ask<T>(
    &self,
    request: Request,
    mut answers: ChannelReceiver<Result<T, StoreError>>,
  ) -> Result<T, OwnerError> {
    match self.requests.try_send(request) {
      Ok(()) => {}
      Err(SyncError::Full(_)) => return Err(OwnerError::Busy),
      Err(SyncError::Closed(_)) => return Err(OwnerError::Gone),
      Err(SyncError::NotOnShardThread(_)) => return Err(OwnerError::NotOnShard),
    }
    match answers.recv().await {
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
    self
      .ask(
        Request::Land {
          header,
          parts,
          answer,
        },
        answers,
      )
      .await
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

  /// The text `identity` of `repository` from `offset`, as far as one piece reaches, made from its chain on the owner
  /// (A-645's derived per-path read form, [`Store::text_at`]): empty at its end, none past it or for a text no landed
  /// changeset kept.
  pub async fn text(
    &self,
    repository: u128,
    identity: Identity,
    offset: u64,
  ) -> Result<Option<Vec<u8>>, OwnerError> {
    let (answer, answers) = sync::channel(1).map_err(|_| OwnerError::Busy)?;
    self
      .ask(
        Request::Text {
          repository,
          identity,
          offset,
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
  /// as it landed ([`Store::text_by_tool`]), in order, none for one no landed changeset named. One batch for many logs,
  /// so a stream asks by its room, never once a log (r20 on hg-src: about 12,000 asks, the serving shard idle half the
  /// stream); bounded by what the asker's room holds.
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
    committed
  }
}

impl Drop for SlicedLanding {
  fn drop(&mut self) {
    if let Some(alive) = self.alive.take() {
      drop(alive);
      // Wakes the owner to see the landing let go. Refused only when its queue is full, and then it has requests to
      // serve, and looks at the landing before each.
      let _ = self.handle.requests.try_send(Request::Dropped);
    }
  }
}
