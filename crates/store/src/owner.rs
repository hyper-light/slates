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
//! **Ending.** When every handle is gone the owner checkpoints the engine at the log's end (so a restart replays
//! nothing) and its thread returns; [`StoreOwner::stop`] joins it.

use std::fmt;

use slates_land::store_root::StoreRoot;
use slates_rt::sync::{self, ChannelReceiver, Sender, SyncError};

use crate::log::Section;
use crate::log::{Header, Located, SectionKind};
use crate::staging::{Identity, Staging};
use crate::store::{Changeset, Contents, PIECE_BYTES, Sizing, Store, StoreError};
use crate::texts::{TextCache, TextRow};

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

/// A request to the owner, with the channel its answer goes back on.
enum Request {
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
}

impl fmt::Debug for Request {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
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
    let (requests, incoming) = sync::channel(waiting).map_err(|_| OwnerError::Busy)?;
    let thread = slates_rt::registry::spawn_waited(
      std::thread::Builder::new().name("slates-store".to_owned()),
      move || serve(root, sizing, incoming),
    )
    .map_err(|_| OwnerError::Gone)?;
    Ok(Self {
      thread,
      handle: StoreHandle { requests },
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

/// The owner's loop: open (recovering), then answer requests until every handle is gone, then checkpoint.
fn serve(
  root: StoreRoot,
  sizing: Sizing,
  mut incoming: ChannelReceiver<Request>,
) -> Result<(), StoreError> {
  let mut store = Store::open(root, sizing)?;
  // The owner lands bytes the request carries; nothing is staged on its side.
  let staging = Staging::new(0);
  // The texts the owner made from their chains (A-645's derived per-path read form), kept across requests, so a clone
  // streaming a path's versions in order makes each from the one before; its bound learned from the chains it made.
  let mut texts = TextCache::new(0);
  while let Ok(request) = incoming.blocking_recv() {
    match request {
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
      Request::Read {
        offset,
        len,
        answer,
      } => {
        let _ = answer.try_send(store.read(offset, len));
      }
    }
  }
  store.checkpoint()
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
      Ok(answer) => answer.map_err(OwnerError::Store),
      Err(SyncError::NotOnShardThread(())) => Err(OwnerError::NotOnShard),
      Err(_) => Err(OwnerError::Gone),
    }
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
