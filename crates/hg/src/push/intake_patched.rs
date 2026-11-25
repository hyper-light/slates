//! A file revision taken a window a step, its text never whole (goal 93, C1 (2); main's order of 2026-11-24 16:10:
//! the push intake keeping no whole text; r618: a long revision rebuilt into one buffer, its pending steps half the 57k
//! intake's time).
//!
//! **How.** Each step makes the text's next window ([`delta::Patching`]) from the revision's delta over its base, read
//! in ranges ([`delta::BaseRead`]): the null base; a revision the fork holds, its metadata block framed before its bytes
//! read through the content store's windows ([`ContentStore::run_at`]), the push's own earlier revisions among them,
//! kept by then; or, where a landed base's windows were let go, its text asked of the node's changeset store between
//! steps and brought back into the content store ([`super::super::held::bring_back_file`]), the answer let go in that
//! same step. The window made is hashed into the node's check and the text's identity, its metadata block read off its
//! front, and its bytes put through the store's streamed put. The base's identity is the text kept last's where it is
//! that one, else hashed a window a step. The last step checks the node and keeps the row: its block, its bytes' object
//! and its length, no text.
//!
//! **Bounded.** A step holds the window it makes and the window of the base it hashes: two windows, whatever the file
//! (`Intaken::most_text_held`). What it keeps between steps is fixed in size, and the metadata block (7.2's copy
//! metadata), charged to the push's admission. It replaces the push's cache of its own texts, the chain rebuild from
//! its own deltas and the text kept last for files ((b1), (b2)): every base a delta names is read from the fork's rows.

use std::ops::Range;

use slates_vcs::{ContentId, ContentStore, Writing};

use super::{Frame, Refused, Stage, Stepped, StoreAsk, Taker, log_of, to_u64};
use crate::delta::{self, BaseRead, Filled, Patching};
use crate::filerev::MARKER;
use crate::framing::Source;
use crate::index::Log;
use crate::node;
use crate::push::verify::Unverified;
use crate::{Admission, NULL_NODE, Node};

/// A file delta's base as a [`Patched`] reads it.
pub(super) enum FileBase {
  /// The null base: no bytes.
  Null,
  /// A revision the fork holds: its framing (the markers around its metadata block, and the block) and its bytes'
  /// object, read a window at a time.
  Stored {
    /// Its node.
    node: Node,
    /// `\1\n`, its block and `\1\n`, where it has a block; empty where it has none.
    framing: Vec<u8>,
    /// Its bytes' object.
    bytes: ContentId,
    /// Their length.
    bytes_len: u64,
  },
}

impl FileBase {
  /// The base `node` of file `path` as the fork holds it: none where it does not, or holds its block not whole.
  fn stored(taker: &Taker, store: &ContentStore, path: &[u8], node: Node) -> Option<FileBase> {
    if node == NULL_NODE {
      return Some(FileBase::Null);
    }
    let log = Log::File(path);
    let rev = taker.fork.rev(store, log, &node)?;
    let row = taker.fork.revision(store, log, rev)?;
    let framing = match row.block {
      Some(block) => {
        let mut framing = MARKER.to_vec();
        for window in store.windows(&block) {
          framing.extend_from_slice(&window);
        }
        framing.extend_from_slice(MARKER);
        framing
      }
      None => Vec::new(),
    };
    let bytes_len = row.len.checked_sub(to_u64(framing.len()))?;
    Some(FileBase::Stored {
      node,
      framing,
      bytes: row.bytes,
      bytes_len,
    })
  }

  /// Its length.
  fn len(&self) -> u64 {
    match self {
      FileBase::Null => 0,
      FileBase::Stored {
        framing, bytes_len, ..
      } => to_u64(framing.len()).saturating_add(*bytes_len),
    }
  }

  /// Its node.
  fn node(&self) -> Node {
    match self {
      FileBase::Null => NULL_NODE,
      FileBase::Stored { node, .. } => *node,
    }
  }
}

/// A [`FileBase`] read through `store` ([`BaseRead`]).
struct Reader<'b> {
  /// The base.
  base: &'b FileBase,
  /// The content store holding its windows.
  store: &'b ContentStore,
}

impl BaseRead for Reader<'_> {
  fn len(&self) -> u64 {
    self.base.len()
  }

  fn read_into(&mut self, at: u64, n: usize, out: &mut Vec<u8>) -> Option<()> {
    let FileBase::Stored {
      framing,
      bytes,
      bytes_len,
      ..
    } = self.base
    else {
      return (n == 0).then_some(());
    };
    let before = out.len();
    let end = at.checked_add(to_u64(n))?;
    let framed = to_u64(framing.len());
    let mut at = at;
    if at < framed {
      let to = end.min(framed);
      out.extend_from_slice(framing.get(usize::try_from(at).ok()?..usize::try_from(to).ok()?)?);
      at = to;
    }
    while at < end {
      let offset = at.saturating_sub(framed);
      let Some(run) = self.store.run_at(bytes, *bytes_len, offset) else {
        out.truncate(before);
        return None;
      };
      let take = usize::try_from(end.saturating_sub(at)).ok()?.min(run.len());
      if take == 0 {
        out.truncate(before);
        return None;
      }
      out.extend_from_slice(run.get(..take)?);
      at = at.saturating_add(to_u64(take));
    }
    Some(())
  }
}

/// Where a file revision's text stands, read off its front as it is made: its metadata block, if any, then its bytes.
enum Framing {
  /// Its first bytes, too few yet to tell whether a block begins it.
  Undecided(Vec<u8>),
  /// Its block being read: `\1\n` and the block's bytes so far.
  Block(Vec<u8>),
  /// Its bytes, its framing read: `\1\n`, the block and `\1\n`, or nothing.
  Bytes(Vec<u8>),
}

/// A file revision being taken a window a step (module docs).
pub(super) struct Patched {
  /// Its file's path.
  path: Vec<u8>,
  /// Its node, first and second parents and link.
  meta: (Node, Node, Node, Node),
  /// Its group's revisions in the changegroup, and its place there: its delta read again each step.
  revisions: Range<usize>,
  /// Its place in its group.
  at: usize,
  /// Where its delta lies in the payload, past a whole's hunk.
  place: Range<u64>,
  /// Its base.
  base: FileBase,
  /// Its base's identity, once known; none for the null base.
  base_identity: Option<[u8; 32]>,
  /// Its base's identity being hashed, and how far.
  base_hash: Option<(blake3::Hasher, u64)>,
  /// The delta applied so far.
  patching: Patching,
  /// Whether its text is made to its end.
  made: bool,
  /// Its node's check.
  checking: node::Checking,
  /// Its identity's hash.
  identity: blake3::Hasher,
  /// Its framing, read so far.
  framing: Framing,
  /// Its bytes being kept.
  writing: Option<Writing>,
  /// The stage after it.
  after: Stage,
}

impl Taker {
  /// Begins taking file revision `revision` of `path` at `at` of `revisions` a window a step ([`Patched`]), its base read
  /// from the fork; passed over (true) where the push brought it or the fork holds it. Refused where its flags or parents
  /// are unknown, or its base is none the fork holds.
  pub(super) fn begin_patched(
    &mut self,
    (path, revision, place): (&[u8], &crate::changegroup::Revision<'_>, Range<u64>),
    (revisions, at, after): (&Range<usize>, usize, Stage),
    (store, admission): (&ContentStore, &mut Admission),
  ) -> Result<bool, Refused> {
    let log = Log::File(path);
    if self.already_brought(log, revision.node, store, admission)? {
      return Ok(true);
    }
    self.check_known(log, revision, store)?;
    let base = FileBase::stored(self, store, path, revision.base).ok_or_else(|| {
      Unverified::UnknownDeltaBase {
        log: log_of(log),
        node: revision.node,
        base: revision.base,
      }
    })?;
    let base_identity = match (&base, &self.last_identity) {
      (FileBase::Null, _) => None,
      (_, Some((node, identity))) if *node == revision.base => Some(*identity),
      _ => None,
    };
    let base_hash = (!matches!(base, FileBase::Null) && base_identity.is_none())
      .then(|| (blake3::Hasher::new(), 0u64));
    // A whole is kept as its raw text (the store's contract, A-767 C1): its place past the one hunk it arrived in.
    let place = if revision.base == NULL_NODE {
      let start = delta::whole_text_start(revision.data).ok_or_else(|| Unverified::NotWhole {
        log: log_of(log),
        node: revision.node,
      })?;
      place.start.saturating_add(to_u64(start))..place.end
    } else {
      place
    };
    let len = delta::patched_len(base.len(), revision.data).map_err(|fault| Unverified::Delta {
      log: log_of(log),
      node: revision.node,
      fault,
    })?;
    if len > to_u64(store.window()) {
      self.intaken.spread = self.intaken.spread.saturating_add(1);
    }
    admission.charge(to_u64(path.len()))?;
    self.patched = Some(Box::new(Patched {
      path: path.to_vec(),
      meta: (revision.node, revision.p1, revision.p2, revision.link),
      revisions: revisions.clone(),
      at,
      place,
      base,
      base_identity,
      base_hash,
      patching: Patching::default(),
      made: false,
      checking: node::Checking::new(&revision.node, &revision.p1, &revision.p2),
      identity: blake3::Hasher::new(),
      framing: Framing::Undecided(Vec::new()),
      writing: None,
      after,
    }));
    Ok(false)
  }

  /// One step of the file revision being taken ([`Patched`]): its delta read again from `payload`, then its base's next
  /// window hashed where its identity is owed and its text's next window made and taken; after its last, its node
  /// checked and its row kept, and the stage moved on.
  pub(super) fn advance_patched<S: Source + ?Sized>(
    &mut self,
    (frame, payload): (&Frame, &S),
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Some(patched) = self.patched.as_ref() else {
      return Ok(Stepped::More);
    };
    let (revisions, at) = (patched.revisions.clone(), patched.at);
    let revision = match frame.placed(payload, &revisions, at) {
      Ok(Some((revision, _))) => revision,
      Ok(None) => {
        return Err(
          Unverified::UnknownDeltaBase {
            log: log_of(Log::File(&patched.path)),
            node: patched.meta.0,
            base: patched.base.node(),
          }
          .into(),
        );
      }
      Err(wanted) => return Ok(Stepped::Wanted(wanted.range)),
    };
    if let Some(stepped) = self.base_brought_back(store)? {
      return Ok(stepped);
    }
    let window = store.window();
    let mut made = self.spares.take();
    made.clear();
    let mut hashed = self.spares.take();
    hashed.clear();
    let filled = self.patched_fill(revision.data, store, window, (&mut made, &mut hashed));
    let held = to_u64(made.len()).saturating_add(to_u64(hashed.len()));
    self.intaken.most_text_held = self.intaken.most_text_held.max(held);
    self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(made.len()));
    self.spares.give(hashed);
    let taken = self.patched_take(&made, store, admission);
    self.spares.give(made);
    taken?;
    match filled? {
      Some(Unread { node }) => self.ask_base(node, admission),
      None => self.patched_finish(store, admission),
    }
  }

  /// The base's next window hashed into its identity where it is owed, and the text's next window made into `made`.
  /// The base's node where its windows were let go, so it is asked of the store; refused as the delta is.
  fn patched_fill(
    &mut self,
    delta: &[u8],
    store: &ContentStore,
    window: usize,
    (made, hashed): (&mut Vec<u8>, &mut Vec<u8>),
  ) -> Result<Option<Unread>, Refused> {
    let Some(patched) = self.patched.as_mut() else {
      return Ok(None);
    };
    let base_len = patched.base.len();
    let mut reader = Reader {
      base: &patched.base,
      store,
    };
    if let Some((hasher, at)) = patched.base_hash.as_mut() {
      let n = usize::try_from(base_len.saturating_sub(*at))
        .unwrap_or(usize::MAX)
        .min(window);
      if reader.read_into(*at, n, hashed).is_none() {
        return Ok(Some(Unread {
          node: patched.base.node(),
        }));
      }
      hasher.update(hashed);
      *at = at.saturating_add(to_u64(n));
      if *at >= base_len {
        patched.base_identity = Some(*hasher.finalize().as_bytes());
        patched.base_hash = None;
      }
    }
    if !patched.made {
      let filled = patched
        .patching
        .fill((delta, &mut reader), window, made)
        .map_err(|fault| Unverified::Delta {
          log: log_of(Log::File(&patched.path)),
          node: patched.meta.0,
          fault,
        })?;
      match filled {
        Filled::More => {}
        Filled::Done => patched.made = true,
        Filled::Unread { .. } => {
          return Ok(Some(Unread {
            node: patched.base.node(),
          }));
        }
      }
    }
    Ok(None)
  }

  /// Takes `made`, the text's next bytes: hashed into the node's check and the identity, its framing read off its front,
  /// its bytes put through the store's streamed put; the block charged to `admission`.
  fn patched_take(
    &mut self,
    made: &[u8],
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<(), Refused> {
    let Some(patched) = self.patched.as_mut() else {
      return Ok(());
    };
    patched.checking.update(made);
    patched.identity.update(made);
    let mut rest = made;
    loop {
      match &mut patched.framing {
        Framing::Undecided(head) => {
          let need = MARKER.len().saturating_sub(head.len());
          let (taken, after) = rest.split_at(need.min(rest.len()));
          head.extend_from_slice(taken);
          rest = after;
          if head.len() < MARKER.len() {
            return Ok(());
          }
          let head = core::mem::take(head);
          if head == MARKER {
            admission.charge(to_u64(head.len()))?;
            patched.framing = Framing::Block(head);
          } else {
            patched.framing = Framing::Bytes(Vec::new());
            put_bytes(&mut patched.writing, store, &head)?;
          }
        }
        Framing::Block(block) => {
          let searched_from = block.len().saturating_sub(1).max(MARKER.len());
          admission.charge(to_u64(rest.len()))?;
          block.extend_from_slice(rest);
          let found = block
            .get(searched_from..)
            .and_then(|tail| tail.windows(MARKER.len()).position(|bytes| bytes == MARKER));
          let Some(found) = found else {
            return Ok(());
          };
          let end = searched_from
            .saturating_add(found)
            .saturating_add(MARKER.len());
          let bytes = block.split_off(end);
          crate::filerev::split(block).map_err(|fault| Unverified::FileRevision {
            path: patched.path.clone(),
            node: patched.meta.0,
            fault,
          })?;
          let framing = core::mem::take(block);
          patched.framing = Framing::Bytes(framing);
          put_bytes(&mut patched.writing, store, &bytes)?;
          return Ok(());
        }
        Framing::Bytes(_) => {
          put_bytes(&mut patched.writing, store, rest)?;
          return Ok(());
        }
      }
    }
  }

  /// Where the base was asked of the store and given: its text brought back into the content store, so its windows read
  /// again, and the answer let go; the step that asked otherwise waits (none where nothing was asked).
  fn base_brought_back(&mut self, store: &mut ContentStore) -> Result<Option<Stepped>, Refused> {
    let Some(patched) = self.patched.as_ref() else {
      return Ok(None);
    };
    let base = patched.base.node();
    let Some(asked) = self.asked.take() else {
      return Ok(None);
    };
    if asked.node != base || asked.path != patched.path {
      self.asked = Some(asked);
      return Ok(None);
    }
    if let Some(refused) = asked.refused {
      return Err(refused);
    }
    let Some(text) = asked.text else {
      self.asked = Some(asked);
      return Ok(Some(Stepped::Store));
    };
    self.intaken.base_store_reads = self.intaken.base_store_reads.saturating_add(1);
    self.intaken.most_text_held = self.intaken.most_text_held.max(to_u64(text.len()));
    let path = patched.path.clone();
    crate::push::held::bring_back_file((&self.fork, store), (&path, &base), &text)
      .map_err(|refused| Refused::Unindexed(super::Unindexed::from(refused)))?;
    self.spares.give(text);
    Ok(None)
  }

  /// Asks the store for base `node`'s text, its windows let go: the step taken again once it is given.
  fn ask_base(&mut self, node: Node, admission: &Admission) -> Result<Stepped, Refused> {
    let Some(patched) = self.patched.as_ref() else {
      return Ok(Stepped::More);
    };
    let path = patched.path.clone();
    self.asked = Some(StoreAsk {
      want: Some(crate::render::Want::Rows {
        logs: vec![(path.clone(), vec![node])],
      }),
      path,
      node,
      most: admission.left(),
      text: None,
      refused: None,
    });
    Ok(Stepped::Store)
  }

  /// After the text's last window and the base's identity: its node checked, its bytes finished, and its row kept, the
  /// stage moved on; else another step.
  fn patched_finish(
    &mut self,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let done = self
      .patched
      .as_ref()
      .is_some_and(|patched| patched.made && patched.base_hash.is_none());
    if !done {
      return Ok(Stepped::More);
    }
    let Some(patched) = self.patched.take() else {
      return Ok(Stepped::More);
    };
    let Patched {
      path,
      meta: (node, p1, p2, link),
      place,
      base_identity,
      patching,
      checking,
      identity,
      framing,
      writing,
      after,
      ..
    } = *patched;
    let log = Log::File(&path);
    // Taken out of the intake, the writer's windows are this step's to give back on any refusal.
    let give_back = |store: &mut ContentStore, writing: Option<Writing>| {
      if let Some(writing) = writing {
        store.abandon_object(writing);
      }
    };
    if let Err(fault) = checking.finish() {
      give_back(store, writing);
      return Err(
        Unverified::Node {
          log: log_of(log),
          node,
          fault,
        }
        .into(),
      );
    }
    let text = match framing {
      // A text shorter than a marker: its few bytes are the text, kept as a short one is.
      Framing::Undecided(head) => head,
      Framing::Block(_) => {
        give_back(store, writing);
        return Err(
          Unverified::FileRevision {
            path: path.clone(),
            node,
            fault: crate::filerev::Refusal::Unclosed,
          }
          .into(),
        );
      }
      Framing::Bytes(framing) => framing,
    };
    let kept = match writing {
      Some(mut writing) => match store.finish_object(&mut writing) {
        Ok(kept) => Some(kept),
        Err(refused) => {
          store.abandon_object(writing);
          return Err(Refused::Unindexed(super::Unindexed::from(refused)));
        }
      },
      None => None,
    };
    let revision = crate::changegroup::Revision {
      node,
      p1,
      p2,
      base: NULL_NODE,
      link,
      flags: 0,
      info: None,
      protocol_flags: 0,
      data: &[],
      sidedata: None,
    };
    let identity = *identity.finalize().as_bytes();
    self.keep_taken(
      log,
      &revision,
      (text, base_identity, place),
      (kept, Some(identity), Some(patching.made())),
      (store, admission),
    )?;
    self.stage = after;
    Ok(Stepped::More)
  }
}

/// A base whose windows were let go, by its node: asked of the store.
struct Unread {
  /// The base's node.
  node: Node,
}

/// Puts `bytes`, a file revision's bytes, through the store's streamed put, its object begun at the first.
fn put_bytes(
  writing: &mut Option<Writing>,
  store: &mut ContentStore,
  bytes: &[u8],
) -> Result<(), Refused> {
  if bytes.is_empty() {
    return Ok(());
  }
  let writing = writing.get_or_insert_with(|| store.begin_object());
  store
    .put_window(writing, bytes)
    .map_err(|refused| Refused::Unindexed(super::Unindexed::from(refused)))
}
