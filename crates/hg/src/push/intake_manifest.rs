//! A pushed manifest taken a window a step, its text never whole (goal 93, C1 (2); main's ruling (B') of 2026-11-24).
//!
//! **How.** Each step makes the manifest's next window ([`delta::Patching`]) from its delta over its base, read front to
//! back over its chain ([`super::super::chain_read`]): a manifest the fork holds, the push's own earlier ones among them;
//! or, where a landed one was let go, its text asked of the node's changeset store between steps and held in the content
//! store as one whole object for the revision's steps, given back after them. The window made is hashed into the node's
//! check and the identity, read line by line as 7.2's reader reads a manifest ([`manifest::Checking`]: every line
//! checked, the entries its delta wrote found from the delta alone, [`delta::Written`], each checked against the files
//! the fork holds and those the push awaits), and kept by the one rule of [`super::super::manifest_row`]: the client's
//! delta where its base is the manifest the fork holds last and the chain allows; else its diff against that last one,
//! made as the window streams ([`crate::manifest_diff::Diffing`], the last one read over its own chain, or the base's
//! where they are one) and put a window a step; else whole. A diff past the chain's bound turns the revision whole: a
//! second pass makes its text again from its delta and puts it, checking nothing twice. The base's identity is the text
//! kept last's where it is that one, else hashed a window a step over a second reading of its chain.
//!
//! **Bounded.** A step holds the window it makes, the window of the base it hashes, and the diff's own (a window and a
//! line of each text, and of its run): `Intaken::most_text_held`. And each chain's deltas while it is read
//! (`Intaken::most_chain_held`, at most its text's length, A-729's bound). No manifest is kept whole but the fork's first
//! and one past its chain's bound, as before (`Intaken::manifests_kept_whole_bytes`).

use std::ops::Range;

use slates_vcs::{ContentId, ContentStore, Writing};

use super::{Frame, ReadAs, Refused, Stage, Stepped, StoreAsk, Streamed, Taker, log_of, to_u64};
use crate::delta::{self, BaseRead, Filled, Patching, Written};
use crate::framing::Source;
use crate::index::Log;
use crate::index::Stored;
use crate::manifest;
use crate::manifest_diff::Diffing;
use crate::node;
use crate::push::chain_read::{Chain, Cursor, Reading};
use crate::push::commit::Unindexed;
use crate::push::manifest_row::{self, Last};
use crate::push::verify::{LogOf, Unverified, audit};
use crate::{Admission, NULL_NODE, Node};

/// A manifest delta's base as a [`Manifesting`] reads it.
enum Base {
  /// The null base: no bytes.
  Null,
  /// A manifest the fork holds, read over its chain.
  Chain {
    /// Its node.
    node: Node,
    /// Its chain.
    chain: Chain,
  },
  /// A manifest let go once landed, its text asked of the store and not yet given.
  Asked {
    /// Its node.
    node: Node,
  },
  /// A manifest's text the store gave, held in the content store as one object for this revision's steps.
  Given {
    /// Its node.
    node: Node,
    /// Its text held whole.
    chain: Chain,
    /// The object, given back after the revision.
    object: ContentId,
  },
}

impl Base {
  /// Its node.
  fn node(&self) -> Node {
    match self {
      Base::Null => NULL_NODE,
      Base::Chain { node, .. } | Base::Asked { node } | Base::Given { node, .. } => *node,
    }
  }

  /// Its chain, where it can be read now.
  fn chain(&self) -> Option<&Chain> {
    match self {
      Base::Chain { chain, .. } | Base::Given { chain, .. } => Some(chain),
      Base::Null | Base::Asked { .. } => None,
    }
  }
}

/// How a manifest's row keeps its content.
enum Keeping {
  /// The client's delta itself, against the row the fork holds last: put a window a step from the changegroup.
  Delta {
    /// That row's number.
    base: u32,
    /// The chain's delta bytes once this one is added.
    chain: u64,
    /// The delta's bytes put so far.
    put: usize,
    /// The delta's object being put.
    writing: Option<Writing>,
  },
  /// Its diff against the manifest the fork holds last, made as the text streams and put a window a step.
  Diff {
    /// That manifest.
    last: Last,
    /// Its text, where it is not the delta's base itself (read then over the base's chain).
    prev: Option<Base>,
    /// The diff's reading of it.
    cursor: Cursor,
    /// The diff being made.
    diffing: Diffing,
    /// The diff's object being put.
    writing: Option<Writing>,
    /// The diff's bytes put so far.
    put: u64,
  },
  /// The text whole, put a window a step as it is made.
  Whole {
    /// The text's object being put.
    writing: Option<Writing>,
  },
}

impl Keeping {
  /// Its object being put.
  fn writing(&mut self) -> &mut Option<Writing> {
    match self {
      Keeping::Delta { writing, .. }
      | Keeping::Diff { writing, .. }
      | Keeping::Whole { writing } => writing,
    }
  }
}

/// What a manifest's step came to ([`Taker::advance_manifest`]).
enum Out {
  /// It waits, or goes on, at its next step.
  Wait(Stepped),
  /// Made, its identity known and its delta put: kept now.
  Done,
  /// A window of its base let go between its steps: begun again.
  Again,
}

/// A manifest being taken a window a step (module docs).
pub(super) struct Manifesting {
  /// Its node, first and second parents and link.
  meta: (Node, Node, Node, Node),
  /// Its place in the manifests' group.
  at: usize,
  /// Where its delta lies in the payload, past a whole's hunk.
  place: Range<u64>,
  /// Its base.
  base: Base,
  /// Its base's identity, once known; none for the null base.
  base_identity: Option<[u8; 32]>,
  /// Its base's identity being hashed: the hash, its own reading of the chain, and how far.
  base_hash: Option<(blake3::Hasher, Cursor, u64)>,
  /// Its text's reading of its base's chain.
  cursor: Cursor,
  /// The delta applied so far.
  patching: Patching,
  /// Whether its text is made to its end.
  made: bool,
  /// Its text's length.
  len: u64,
  /// Its node's check, until its first pass ends.
  checking: Option<node::Checking>,
  /// Its identity's hash, until its first pass ends.
  identity: Option<blake3::Hasher>,
  /// Its identity, once hashed.
  hashed: Option<[u8; 32]>,
  /// Its row's stored fields, once decided.
  stored: Option<Stored>,
  /// Whether its text is being made again to be kept whole: its diff passed the chain's bound.
  remaking: bool,
  /// The manifest whose chain could not be read: asked of the store when it is begun again.
  unread: Option<Node>,
  /// Its text, where it is one window at most: kept whole from it, without a second pass, where its diff passes the
  /// chain's bound (a window is what a step holds anyway).
  small: Option<Vec<u8>>,
  /// Its lines' check.
  lines: manifest::Checking,
  /// The ranges its delta wrote.
  written: Written,
  /// Whether every line is written: a whole, against the null base.
  every: bool,
  /// The file revisions its written entries name that the fork lacks and the push awaits nowhere yet.
  awaited: Vec<(Vec<u8>, Node)>,
  /// How its row keeps its content.
  keeping: Keeping,
  /// The stage after it.
  after: Stage,
}

impl Taker {
  /// Begins taking manifest `revision` at `at` of the manifests' group a window a step ([`Manifesting`]); passed over
  /// (true) where the push brought it or the fork holds it. Refused where its flags or parents are unknown, or its base
  /// is none the fork holds.
  pub(super) fn begin_manifest(
    &mut self,
    (revision, place): (&crate::changegroup::Revision<'_>, Range<u64>),
    (at, after): (usize, Stage),
    (store, admission): (&ContentStore, &mut Admission),
  ) -> Result<bool, Refused> {
    let log = Log::Manifest;
    if self.already_brought(log, revision.node, store, admission)? {
      return Ok(true);
    }
    self.check_known(log, revision, store)?;
    let unknown = || Unverified::UnknownDeltaBase {
      log: LogOf::Manifests,
      node: revision.node,
      base: revision.base,
    };
    let (base, base_len) = if revision.base == NULL_NODE {
      (Base::Null, 0)
    } else {
      self.read_base(store, revision.base).ok_or_else(unknown)?
    };
    let base_identity = match (&base, &self.last_identity) {
      (Base::Null, _) => None,
      (_, Some((node, identity))) if *node == revision.base => Some(*identity),
      _ => None,
    };
    let len = delta::patched_len(base_len, revision.data).map_err(|fault| Unverified::Delta {
      log: LogOf::Manifests,
      node: revision.node,
      fault,
    })?;
    // A whole is kept as its raw text (the store's contract, A-767 C1): its place past the one hunk it arrived in.
    let place = if revision.base == NULL_NODE {
      let start = delta::whole_text_start(revision.data).ok_or_else(|| Unverified::NotWhole {
        log: LogOf::Manifests,
        node: revision.node,
      })?;
      place.start.saturating_add(to_u64(start))..place.end
    } else {
      place
    };
    let keeping = self.keeping_of(revision, len, store);
    if len > to_u64(store.window()) {
      self.intaken.spread = self.intaken.spread.saturating_add(1);
    }
    let every = revision.is_full_text() || revision.base == NULL_NODE;
    let base_hash = (!matches!(base, Base::Null) && base_identity.is_none())
      .then(|| (blake3::Hasher::new(), Cursor::default(), 0u64));
    self.manifesting = Some(Box::new(Manifesting {
      meta: (revision.node, revision.p1, revision.p2, revision.link),
      at,
      place,
      base,
      base_identity,
      base_hash,
      cursor: Cursor::default(),
      patching: Patching::default(),
      made: false,
      len,
      checking: Some(node::Checking::new(
        &revision.node,
        &revision.p1,
        &revision.p2,
      )),
      identity: Some(blake3::Hasher::new()),
      hashed: None,
      stored: None,
      remaking: false,
      unread: None,
      small: (len <= to_u64(store.window())).then(Vec::new),
      lines: manifest::Checking::default(),
      written: Written::default(),
      every,
      awaited: Vec::new(),
      keeping,
      after,
    }));
    Ok(false)
  }

  /// How manifest `node` of the fork is read: over its chain where its text is at hand, else asked of the store (and
  /// asked whatever it is where its chain could not be read before, [`Out::Again`]); its text's length. None where the
  /// fork holds no such manifest.
  fn read_base(&mut self, store: &ContentStore, node: Node) -> Option<(Base, u64)> {
    let log = Log::Manifest;
    let rev = self.fork.rev(store, log, &node)?;
    let (len, at_hand) = crate::push::held::log_text_at_hand((&self.fork, store), log, &node)?;
    let asked = self.base_asked.as_ref().is_some_and(|asked| *asked == node);
    if asked {
      self.base_asked = None;
    }
    Some(
      match Chain::of(&self.fork, store, rev).filter(|_| at_hand && !asked) {
        Some(chain) => (Base::Chain { node, chain }, len),
        None => (Base::Asked { node }, len),
      },
    )
  }

  /// How `revision`'s row keeps its content, `len` its text's length, by [`manifest_row`]'s rule: its client's delta;
  /// else its diff against the manifest the fork holds last; else whole, the fork's first.
  fn keeping_of(
    &mut self,
    revision: &crate::changegroup::Revision<'_>,
    len: u64,
    store: &ContentStore,
  ) -> Keeping {
    let last = manifest_row::last(&self.fork, store);
    let sent = (revision.base != NULL_NODE && !revision.is_full_text())
      .then(|| {
        manifest_row::as_sent(
          last.as_ref(),
          &revision.base,
          to_u64(revision.data.len()),
          len,
        )
      })
      .flatten();
    if let Some((base, _, chain)) = sent {
      return Keeping::Delta {
        base,
        chain,
        put: 0,
        writing: None,
      };
    }
    let Some(last) = last else {
      return Keeping::Whole { writing: None };
    };
    let prev = if last.node == revision.base {
      None
    } else {
      self.read_base(store, last.node).map(|(base, _)| base)
    };
    Keeping::Diff {
      last,
      prev,
      cursor: Cursor::default(),
      diffing: Diffing::within(store.window()),
      writing: None,
      put: 0,
    }
  }

  /// One step of the manifest being taken ([`Manifesting`]): its delta read again from `payload`; its base brought in
  /// where the store gave it; then its base's next window hashed where its identity is owed, its text's next window made
  /// and taken, and its delta's next window put where its row keeps it; after its last, its node and lines checked and
  /// its row kept, and the stage moved on.
  pub(super) fn advance_manifest<S: Source + ?Sized>(
    &mut self,
    (frame, payload): (&Frame, &S),
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let Some(mut manifesting) = self.manifesting.take() else {
      return Ok(Stepped::More);
    };
    let stepped = self.manifest_step(&mut manifesting, (frame, payload), store, admission);
    match stepped {
      Ok(Out::Wait(stepped)) => {
        self.manifesting = Some(manifesting);
        Ok(stepped)
      }
      // Done: kept, or refused with what it held given back.
      Ok(Out::Done) => self.manifest_finish(*manifesting, store, admission),
      // A window of its base let go between its steps: what it took given back, and begun again at its next step, which
      // asks the store for its base's text.
      Ok(Out::Again) => {
        // Begun again with its base asked of the store, never read over its chain a second time, so it is begun again
        // once at most.
        self.base_asked = Some(
          manifesting
            .unread
            .unwrap_or_else(|| manifesting.base.node()),
        );
        give_back(store, *manifesting);
        Ok(Stepped::More)
      }
      Err(refused) => {
        give_back(store, *manifesting);
        Err(refused)
      }
    }
  }

  /// [`Taker::advance_manifest`]'s step ([`Out`]).
  fn manifest_step<S: Source + ?Sized>(
    &mut self,
    manifesting: &mut Manifesting,
    (frame, payload): (&Frame, &S),
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Out, Refused> {
    let revision = match frame.placed(payload, &frame.0.manifests, manifesting.at) {
      Ok(Some((revision, _))) => revision,
      Ok(None) => {
        return Err(
          Unverified::UnknownDeltaBase {
            log: LogOf::Manifests,
            node: manifesting.meta.0,
            base: manifesting.base.node(),
          }
          .into(),
        );
      }
      Err(wanted) => return Ok(Out::Wait(Stepped::Wanted(wanted.range))),
    };
    // A base, or a last manifest a diff reads, let go: asked of the store and brought in first.
    if let Some(stepped) = self.bring_in(&mut manifesting.base, store, admission)? {
      return Ok(Out::Wait(stepped));
    }
    if let Keeping::Diff {
      prev: Some(prev), ..
    } = &mut manifesting.keeping
      && let Some(stepped) = self.bring_in(prev, store, admission)?
    {
      return Ok(Out::Wait(stepped));
    }
    let window = store.window();
    let mut made = self.spares.take();
    made.clear();
    let mut hashed = self.spares.take();
    hashed.clear();
    let filled = fill(
      manifesting,
      (revision.data, &*store),
      window,
      (&mut made, &mut hashed),
    );
    let chain_held = manifesting.base.chain().map_or(0, Chain::held);
    self.intaken.most_chain_held = self.intaken.most_chain_held.max(chain_held);
    let held = to_u64(made.len())
      .saturating_add(to_u64(hashed.len()))
      .saturating_add(diff_held(manifesting));
    self.intaken.most_text_held = self.intaken.most_text_held.max(held);
    self.intaken.longest_step_bytes = self.intaken.longest_step_bytes.max(to_u64(made.len()));
    self.spares.give(hashed);
    let taken = self.manifest_take(manifesting, (&made, revision.data), store, admission);
    self.spares.give(made);
    let unread = taken?;
    put_delta(manifesting, revision.data, store, window)?;
    if let Some(node) = filled?.or(unread) {
      manifesting.unread = Some(node);
      return Ok(Out::Again);
    }
    let put = match &manifesting.keeping {
      Keeping::Delta { put, .. } => *put >= revision.data.len(),
      Keeping::Diff { .. } | Keeping::Whole { .. } => true,
    };
    if !(manifesting.made && manifesting.base_hash.is_none() && put) {
      return Ok(Out::Wait(Stepped::More));
    }
    if manifesting.remaking {
      return Ok(Out::Done);
    }
    self.first_pass_ended(manifesting, (revision.data, store))
  }

  /// The first pass made: its node and lines checked, its identity hashed, and its row's stored fields decided by
  /// [`manifest_row`]'s rule. A diff past the chain's bound gives its object back and makes the text again to keep it
  /// whole ([`Manifesting::remaking`]); else it is kept now.
  fn first_pass_ended(
    &mut self,
    manifesting: &mut Manifesting,
    (delta, store): (&[u8], &mut ContentStore),
  ) -> Result<Out, Refused> {
    let node = manifesting.meta.0;
    if let Some(checking) = manifesting.checking.take() {
      checking.finish().map_err(|fault| Unverified::Node {
        log: LogOf::Manifests,
        node,
        fault,
      })?;
    }
    core::mem::take(&mut manifesting.lines)
      .finish()
      .map_err(|fault| Unverified::Manifest { node, fault })?;
    manifesting.hashed = manifesting
      .identity
      .take()
      .map(|identity| *identity.finalize().as_bytes());
    let len = manifesting.len;
    let stored = match &mut manifesting.keeping {
      Keeping::Delta { base, chain, .. } => Some((*base, to_u64(delta.len()), *chain)),
      Keeping::Whole { .. } => Some(manifest_row::WHOLE),
      Keeping::Diff {
        last,
        prev,
        cursor,
        diffing,
        writing,
        put,
      } => {
        let chain = match prev.as_ref() {
          Some(prev) => prev.chain(),
          None => manifesting.base.chain(),
        };
        let Some(chain) = chain else {
          return Err(unread_of(node, last.node));
        };
        let mut reading = Reading {
          chain,
          cursor,
          store: &*store,
        };
        let mut out = Vec::new();
        let made = core::mem::take(diffing).finish(&mut reading, &mut out);
        if made.is_err() {
          manifesting.unread = Some(last.node);
          return Ok(Out::Again);
        }
        put_window(writing, store, &out)?;
        *put = put.saturating_add(to_u64(out.len()));
        manifest_row::diffed(last, *put, len)
      }
    };
    match stored {
      Some(stored) => {
        manifesting.stored = Some(stored);
        Ok(Out::Done)
      }
      // Its diff passed the chain's bound: kept whole, its text made again from its delta, nothing checked twice; or, a
      // text of one window, put from what the first pass kept of it.
      None => {
        if let Some(writing) = manifesting.keeping.writing().take() {
          store.abandon_object(writing);
        }
        if let Some(small) = manifesting.small.take() {
          if let Keeping::Diff {
            prev: Some(Base::Given { object, .. }),
            ..
          } = &manifesting.keeping
          {
            given_back(store, object);
          }
          let mut writing = None;
          put_window(&mut writing, store, &small)?;
          manifesting.keeping = Keeping::Whole { writing };
          manifesting.stored = Some(manifest_row::WHOLE);
          return Ok(Out::Done);
        }
        if let Keeping::Diff {
          prev: Some(Base::Given { object, .. }),
          ..
        } = &manifesting.keeping
        {
          given_back(store, object);
        }
        manifesting.keeping = Keeping::Whole { writing: None };
        manifesting.stored = Some(manifest_row::WHOLE);
        manifesting.remaking = true;
        manifesting.made = false;
        manifesting.patching = Patching::default();
        manifesting.cursor = Cursor::default();
        Ok(Out::Wait(Stepped::More))
      }
    }
  }

  /// Where `base` was let go: its text asked of the store (`Store`), or, given, held in the content store whole for the
  /// revision's steps and read from there (`More`). None where it can be read now.
  fn bring_in(
    &mut self,
    base: &mut Base,
    store: &mut ContentStore,
    admission: &Admission,
  ) -> Result<Option<Stepped>, Refused> {
    let Base::Asked { node } = *base else {
      return Ok(None);
    };
    let asked = self
      .asked
      .take()
      .filter(|asked| asked.node == node && asked.path == super::MANIFEST_PATH);
    let Some(asked) = asked else {
      self.asked = Some(StoreAsk {
        path: super::MANIFEST_PATH.to_vec(),
        node,
        want: Some(crate::render::Want::Rows {
          logs: vec![(super::MANIFEST_PATH.to_vec(), vec![node])],
        }),
        most: admission.left(),
        text: None,
        refused: None,
      });
      return Ok(Some(Stepped::Store));
    };
    if let Some(refused) = asked.refused {
      return Err(refused);
    }
    let Some(text) = asked.text else {
      self.asked = Some(asked);
      return Ok(Some(Stepped::Store));
    };
    self.intaken.log_texts_asked = self.intaken.log_texts_asked.saturating_add(1);
    self.intaken.most_text_held = self.intaken.most_text_held.max(to_u64(text.len()));
    let object = store
      .put_object(&text)
      .map_err(|refused| Refused::Unindexed(Unindexed::from(refused)))?;
    let len = to_u64(text.len());
    self.spares.give(text);
    *base = Base::Given {
      node,
      chain: Chain::whole(object, len),
      object,
    };
    Ok(Some(Stepped::More))
  }

  /// Takes `made`, the text's next bytes, `delta` its delta: on its first pass hashed into the node's check and the
  /// identity, read line by line (each entry the delta wrote checked against the files the fork holds and those
  /// awaited, a new one's path charged to a copy of `admission`) and fed to its diff; put where the row keeps the text.
  /// The manifest whose chain the diff could not read, if any.
  fn manifest_take(
    &self,
    manifesting: &mut Manifesting,
    (made, delta): (&[u8], &[u8]),
    store: &mut ContentStore,
    admission: &Admission,
  ) -> Result<Option<Node>, Refused> {
    if manifesting.remaking {
      if let Keeping::Whole { writing } = &mut manifesting.keeping {
        put_window(writing, store, made)?;
      }
      return Ok(None);
    }
    if let Some(checking) = manifesting.checking.as_mut() {
      checking.update(made);
    }
    if let Some(small) = manifesting.small.as_mut() {
      small.extend_from_slice(made);
    }
    if let Some(identity) = manifesting.identity.as_mut() {
      identity.update(made);
    }
    let node = manifesting.meta.0;
    let Manifesting {
      lines,
      written,
      every,
      awaited,
      keeping,
      base,
      ..
    } = manifesting;
    let mut refused: Option<Refused> = None;
    let mut room = *admission;
    let read = lines.update(made, &mut |range, entry| {
      if refused.is_some() {
        return Ok(());
      }
      let touched = match *every {
        true => true,
        false => match written.touches(delta, range) {
          Ok(touched) => touched,
          Err(fault) => {
            refused = Some(
              Unverified::Delta {
                log: LogOf::Manifests,
                node,
                fault,
              }
              .into(),
            );
            return Ok(());
          }
        },
      };
      if !touched {
        return Ok(());
      }
      if let Err(fault) = self.awaiting(entry, awaited, &mut room, store) {
        refused = Some(fault);
      }
      Ok(())
    });
    read.map_err(|fault| Unverified::Manifest { node, fault })?;
    if let Some(refused) = refused {
      return Err(refused);
    }
    match keeping {
      Keeping::Whole { writing } => put_window(writing, store, made)?,
      Keeping::Diff {
        last,
        prev,
        cursor,
        diffing,
        writing,
        put,
      } => {
        let chain = match prev.as_ref() {
          Some(prev) => prev.chain(),
          None => base.chain(),
        };
        let Some(chain) = chain else {
          return Ok(Some(last.node));
        };
        let mut reading = Reading {
          chain,
          cursor,
          store: &*store,
        };
        let mut out = Vec::new();
        if diffing.update(made, &mut reading, &mut out).is_err() {
          return Ok(Some(last.node));
        }
        put_window(writing, store, &out)?;
        *put = put.saturating_add(to_u64(out.len()));
      }
      Keeping::Delta { .. } => {}
    }
    Ok(None)
  }

  /// Notes `entry`, one a manifest's delta wrote, as awaited where the fork lacks its file revision and the push awaits it
  /// nowhere yet (`Taker::awaited_of`'s rule): its path audited and charged to `room`, refused past what the push brings.
  fn awaiting(
    &self,
    entry: manifest::Entry<'_>,
    awaited: &mut Vec<(Vec<u8>, Node)>,
    room: &mut Admission,
    store: &ContentStore,
  ) -> Result<(), Refused> {
    audit(entry.path)?;
    let path = entry.path.to_vec();
    let held = self
      .fork
      .rev(store, Log::File(&path), &entry.node)
      .is_some();
    let key = (path, entry.node);
    if held || self.awaited.contains(&key) || awaited.contains(&key) {
      return Ok(());
    }
    if to_u64(self.awaited.len().saturating_add(awaited.len())) >= self.bringing {
      return Err(
        Unverified::MissingFile {
          path: key.0,
          node: key.1,
        }
        .into(),
      );
    }
    room.charge(to_u64(key.0.len()))?;
    awaited.push(key);
    Ok(())
  }

  /// After the manifest's last pass: its content's object finished and its row kept as decided
  /// ([`Taker::first_pass_ended`]; `Taker::keep_taken`, a manifest kept whole counted), the stage moved on. Refused,
  /// what it held is given back.
  fn manifest_finish(
    &mut self,
    mut manifesting: Manifesting,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Stepped, Refused> {
    let writing = manifesting.keeping.writing().take();
    let kept = match writing {
      Some(mut writing) => match store.finish_object(&mut writing) {
        Ok(kept) => kept,
        Err(refused) => {
          store.abandon_object(writing);
          give_back(store, manifesting);
          return Err(Refused::Unindexed(Unindexed::from(refused)));
        }
      },
      // Nothing put: the empty manifest, or a diff of nothing, kept as the empty object.
      None => match store.put_object(&[]) {
        Ok(kept) => kept,
        Err(refused) => {
          give_back(store, manifesting);
          return Err(Refused::Unindexed(Unindexed::from(refused)));
        }
      },
    };
    let stored = manifesting.stored.unwrap_or(manifest_row::WHOLE);
    let len = manifesting.len;
    let (node, p1, p2, link) = manifesting.meta;
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
    let awaited = core::mem::take(&mut manifesting.awaited);
    let kept_taken = self.keep_taken(
      Log::Manifest,
      &revision,
      (
        Vec::new(),
        manifesting.base_identity,
        manifesting.place.clone(),
      ),
      Streamed {
        kept: Some(kept),
        identity: manifesting.hashed,
        len: Some(len),
        stored: Some(stored),
        read: Some(ReadAs::Manifest { awaited }),
      },
      (store, admission),
    );
    // The row holds its content again, and the put's own hold is given back with the keep's (`keep_one`); a given base's
    // and a given last manifest's are given back either way.
    let after = manifesting.after;
    give_back(store, manifesting);
    kept_taken?;
    // Counted once kept, so a keep refused past its admission and taken again counts it once.
    if stored == manifest_row::WHOLE {
      self.intaken.manifests_kept_whole_bytes =
        self.intaken.manifests_kept_whole_bytes.saturating_add(len);
    }
    self.stage = after;
    Ok(Stepped::More)
  }
}

/// The bytes a manifest's diff holds now ([`Diffing::held`]).
fn diff_held(manifesting: &Manifesting) -> u64 {
  match &manifesting.keeping {
    Keeping::Diff { diffing, .. } => diffing.held(),
    Keeping::Delta { .. } | Keeping::Whole { .. } => 0,
  }
}

/// The refusal of a manifest whose last manifest's text could not be read.
fn unread_of(node: Node, base: Node) -> Refused {
  Unverified::UnknownDeltaBase {
    log: LogOf::Manifests,
    node,
    base,
  }
  .into()
}

/// The base's next window hashed into its identity where it is owed, and the text's next window made into `made`, from
/// `delta` over the base's chain read through `store`. The base's node where a window of its chain was let go.
fn fill(
  manifesting: &mut Manifesting,
  (delta, store): (&[u8], &ContentStore),
  window: usize,
  (made, hashed): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<Option<Node>, Refused> {
  let node = manifesting.base.node();
  let Manifesting {
    base,
    base_identity,
    base_hash,
    cursor,
    patching,
    made: done,
    meta,
    ..
  } = manifesting;
  let base: &Base = base;
  let empty = Chain::whole(slates_vcs::ContentId::of(&[]), 0);
  let chain = match base {
    Base::Null => &empty,
    Base::Chain { chain, .. } | Base::Given { chain, .. } => chain,
    Base::Asked { .. } => return Ok(None),
  };
  if let Some((hasher, hash_cursor, at)) = base_hash.as_mut() {
    let len = chain.len();
    let n = usize::try_from(len.saturating_sub(*at))
      .unwrap_or(usize::MAX)
      .min(window);
    let mut reading = Reading {
      chain,
      cursor: hash_cursor,
      store,
    };
    if reading.read_into(*at, n, hashed).is_none() {
      return Ok(Some(node));
    }
    hasher.update(hashed);
    *at = at.saturating_add(to_u64(n));
    if *at >= len {
      *base_identity = Some(*hasher.finalize().as_bytes());
      *base_hash = None;
    }
  }
  if !*done {
    let mut reading = Reading {
      chain,
      cursor,
      store,
    };
    let mut null: &[u8] = &[];
    let reader: &mut dyn BaseRead = match base {
      Base::Null => &mut null,
      _ => &mut reading,
    };
    let filled = patching
      .fill((delta, reader), window, made)
      .map_err(|fault| Unverified::Delta {
        log: log_of(Log::Manifest),
        node: meta.0,
        fault,
      })?;
    match filled {
      Filled::More => {}
      Filled::Done => *done = true,
      Filled::Unread { .. } => return Ok(Some(node)),
    }
  }
  Ok(None)
}

/// Puts the delta's next window where the row keeps the client's delta.
fn put_delta(
  manifesting: &mut Manifesting,
  delta: &[u8],
  store: &mut ContentStore,
  window: usize,
) -> Result<(), Refused> {
  let Keeping::Delta { put, writing, .. } = &mut manifesting.keeping else {
    return Ok(());
  };
  let end = delta.len().min(put.saturating_add(window));
  if let Some(bytes) = delta.get(*put..end) {
    put_window(writing, store, bytes)?;
  }
  *put = end;
  Ok(())
}

/// Puts `bytes` through the store's streamed put, the object begun at the first.
fn put_window(
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
    .map_err(|refused| Refused::Unindexed(Unindexed::from(refused)))
}

/// Gives back what a manifest being taken holds: its object being put, and the objects of a base and a last manifest the
/// store gave.
pub(super) fn give_back(store: &mut ContentStore, mut manifesting: Manifesting) {
  if let Some(writing) = manifesting.keeping.writing().take() {
    store.abandon_object(writing);
  }
  if let Base::Given { object, .. } = manifesting.base {
    given_back(store, &object);
  }
  if let Keeping::Diff {
    prev: Some(Base::Given { object, .. }),
    ..
  } = manifesting.keeping
  {
    given_back(store, &object);
  }
}

/// Gives back a manifest's text the store gave, held as one object for a revision's steps: its hold released, then let
/// go, so where its text is a landed row's own (a manifest kept whole), that row's object, brought back by the put, goes
/// again as its landing let it go.
fn given_back(store: &mut ContentStore, object: &ContentId) {
  drop(store.release(object));
  store.let_go_object(object);
}
