//! Content by identity (§4.17.1 "Objects" item 1; §4.11; D-6): every object's raw bytes, the bytes its tool hashes to
//! name it, are kept once, named by their BLAKE3 identity, however many objects, tools and repositories name them.
//!
//! **One copy.** A put hashes the bytes and, when the store already holds that identity, counts one more holder and
//! stores nothing ([`ContentCounters::deduplicated`], the non-vacuity counter the tests read). So a git blob, the same
//! file's Mercurial file revision and a Subversion text are one content, and a fork's objects are its parent's. The
//! identity is always the store's own hash of the bytes it keeps: a caller cannot name content by an identity it
//! computed, so a stored content always holds the bytes its identity names.
//!
//! **One window at most.** The content plane keeps content in page-multiple chunks no longer than its window (§2.3's
//! chunk arenas; D-6; sixteen base pages, `slates_vfs::content::chunk_bytes`), and the owner gives the store that
//! window when it makes it ([`ContentStore::new`]): no content the store holds is longer. An object longer than a
//! window is cut at the offsets a volume cuts a file's bytes at, `0, w, 2w, …` (`slates_vfs`'s `window_of`), so a
//! window of an object and the same window of the same file in a version's tree have one identity: the condition for
//! §4.17.1's "a checkout costs metadata and no bytes" once one content index holds both (GAPS). Its windows are named
//! by a list of their identities, and a list longer than a window by lists of lists, up to one root
//! ([`ContentStore::put_object`]); a list holds each identity it names once for each time it names it. An object of at
//! most one window is that window, so its root is the plain BLAKE3 of its bytes.
//!
//! **Identities by form** ([`Form`]). A window is named by the plain BLAKE3 of its bytes, as a volume's sealed chunk
//! and an archive's chunk are (§4.11). A list and an index's table ([`crate::table`]) are named by BLAKE3 in its
//! key-derivation mode under a context of their own [B: the `blake3` crate's `derive_key`, "a hardcoded, globally
//! unique, and application-specific context string"], so no bytes a client sends can be taken for either. Without it a
//! pushed window equal to a list's bytes would be read as the identities it spells, and one equal to a table's bytes
//! would keep the table's content alive past the table, and with it every content the table's rows held.
//!
//! **Held by count.** Each holder (a list, a tail row, a table, a quarantined object; see the crate's docs) takes one
//! count and gives it back once. A content is freed with its last count; a freed list gives back what it named, a
//! level at a time, and [`ContentStore::release`] hands the freed bytes back, so a caller whose content names other
//! content (a table naming its rows' objects) can release what it named.
//!
//! **Within a budget.** The store counts the bytes it keeps, and the bytes structures beside it charge to it (an
//! index's tail and its runs' fences, [`ContentStore::charge`]), against a budget the caller derives from its shard's
//! memory (§4.2) and may derive again as pressure moves ([`ContentStore::set_budget`]). A put or a charge past it is
//! refused, [`ContentRefused::OverBudget`], with nothing changed; an object's whole tree, and an index's tables, are
//! admitted as one step before a byte of them is kept. A lowered budget refuses new bytes until releases bring the
//! store under it, and never drops what is held.
//!
//! The map is ordered (`BTreeMap`) so that every walk of it is deterministic.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

/// Format: a content identity's bytes, BLAKE3's output length.
pub const IDENTITY_BYTES: usize = blake3::OUT_LEN;

/// Format: the context a list's identity is derived under ([`Form::List`]), in the form the `blake3` crate's
/// `derive_key` documents: "[application] [commit timestamp] [purpose]".
const LIST_CONTEXT: &str =
  "slates-vcs 2026-11-19 object store: the identities of an object's windows";

/// Format: the context a table's identity is derived under ([`Form::Table`]).
const TABLE_CONTEXT: &str = "slates-vcs 2026-11-19 object store: an index table";

/// A content identity: the BLAKE3 of the content's bytes, in the mode its [`Form`] names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId(pub [u8; IDENTITY_BYTES]);

/// What a content's bytes are, which decides how its identity is hashed and what freeing it gives back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
  /// A window of an object's raw bytes, or a whole object of at most one window: the plain BLAKE3 of its bytes.
  Window,
  /// The identities of an object's windows, or of lists, in order; it holds each.
  List,
  /// An index's table ([`crate::table`]).
  Table,
}

impl ContentId {
  /// The identity of the window `bytes`: their plain BLAKE3.
  pub fn of(bytes: &[u8]) -> ContentId {
    ContentId(*blake3::hash(bytes).as_bytes())
  }

  /// The identity of a content of `form` whose bytes are `bytes`.
  pub fn named(form: Form, bytes: &[u8]) -> ContentId {
    let context = match form {
      Form::Window => return ContentId::of(bytes),
      Form::List => LIST_CONTEXT,
      Form::Table => TABLE_CONTEXT,
    };
    let mut hasher = blake3::Hasher::new_derive_key(context);
    hasher.update(bytes);
    ContentId(*hasher.finalize().as_bytes())
  }

  /// The identity whose bytes are `bytes`, or `None` when `bytes` is not an identity's length.
  pub fn from_slice(bytes: &[u8]) -> Option<ContentId> {
    <[u8; IDENTITY_BYTES]>::try_from(bytes).ok().map(ContentId)
  }
}

/// Why the store refused a request. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentRefused {
  /// Keeping `need` more bytes would pass the budget, which has `room` left.
  OverBudget {
    /// The bytes the request needed.
    need: u64,
    /// The bytes the budget had left.
    room: u64,
  },
  /// The store holds no content of the identity named.
  NotHeld,
  /// A holder count, or a byte count, would pass `u64::MAX`.
  CountExhausted,
  /// A discharge named more bytes than were charged.
  NotCharged,
  /// `len` bytes are longer than one window: an object that long is put through [`ContentStore::put_object`], and no
  /// table is cut that long.
  PastWindow {
    /// The bytes' length.
    len: u64,
    /// The window.
    window: u64,
  },
  /// A window too short for a list of two identities, under which no object longer than a window could be named.
  WindowTooSmall {
    /// The window asked for.
    window: u64,
  },
}

/// What the store has done, for the tests' non-vacuity checks and the owner's accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentCounters {
  /// Contents stored new.
  pub stored: u64,
  /// Puts answered by a content already held: bytes not stored again.
  pub deduplicated: u64,
  /// Contents freed with their last holder.
  pub freed: u64,
  /// Lists stored new: objects longer than a window held as their windows.
  pub lists: u64,
}

/// What a put did: the content's identity, and whether its bytes were stored new or were held already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
  /// The identity.
  pub identity: ContentId,
  /// Whether the bytes were stored new.
  pub fresh: bool,
}

/// One content: its bytes, its form and its holders.
#[derive(Debug)]
struct Held {
  bytes: Box<[u8]>,
  form: Form,
  holders: u64,
}

/// The content store (module docs).
#[derive(Debug)]
pub struct ContentStore {
  entries: BTreeMap<ContentId, Held>,
  /// The longest content the store holds: the content plane's window.
  window: NonZeroUsize,
  /// The identities one list names at most: the window over an identity's bytes, at least two.
  fanout: NonZeroUsize,
  budget: u64,
  /// The bytes of every content held.
  content_bytes: u64,
  /// The bytes structures beside the store charged to it.
  charged_bytes: u64,
  counters: ContentCounters,
}

/// A byte count as the store keeps it.
fn width(len: usize) -> Result<u64, ContentRefused> {
  u64::try_from(len).map_err(|_| ContentRefused::CountExhausted)
}

/// One node of an object's tree as planned: its identity, and its bytes, a window of the object's or a list's own.
#[derive(Debug)]
struct Node<'b> {
  identity: ContentId,
  bytes: NodeBytes<'b>,
}

/// A planned node's bytes.
#[derive(Debug)]
enum NodeBytes<'b> {
  /// A window of the object's bytes.
  Window(&'b [u8]),
  /// A list: the identities of the nodes a level below, `first..first + count` of that level.
  List { bytes: Vec<u8>, first: usize },
}

impl Node<'_> {
  fn bytes(&self) -> &[u8] {
    match &self.bytes {
      NodeBytes::Window(bytes) => bytes,
      NodeBytes::List { bytes, .. } => bytes,
    }
  }

  fn form(&self) -> Form {
    match self.bytes {
      NodeBytes::Window(_) => Form::Window,
      NodeBytes::List { .. } => Form::List,
    }
  }
}

/// An object's tree as planned (module docs): its levels from the windows up to the root alone.
#[derive(Debug)]
struct Tree<'b> {
  levels: Vec<Vec<Node<'b>>>,
}

impl<'b> Tree<'b> {
  /// The tree of `bytes` cut into `window`-byte windows and lists of at most `fanout` identities.
  fn plan(bytes: &'b [u8], window: NonZeroUsize, fanout: NonZeroUsize) -> Tree<'b> {
    let mut windows: Vec<Node<'b>> = bytes
      .chunks(window.get())
      .map(|piece| Node {
        identity: ContentId::of(piece),
        bytes: NodeBytes::Window(piece),
      })
      .collect();
    if windows.is_empty() {
      windows.push(Node {
        identity: ContentId::of(bytes),
        bytes: NodeBytes::Window(bytes),
      });
    }
    let mut levels = vec![windows];
    while let Some(below) = levels.last().filter(|below| below.len() > 1) {
      let above = Tree::lists_over(below, fanout);
      // A list names at least two identities, so each level is shorter than the one below it; the check keeps the
      // walk finite whatever a caller's window were.
      if above.len() >= below.len() {
        break;
      }
      levels.push(above);
    }
    Tree { levels }
  }

  /// The lists naming `below`, `fanout` identities a list at most.
  fn lists_over(below: &[Node<'_>], fanout: NonZeroUsize) -> Vec<Node<'b>> {
    below
      .chunks(fanout.get())
      .enumerate()
      .map(|(at, group)| {
        let mut bytes = Vec::with_capacity(group.len().saturating_mul(IDENTITY_BYTES));
        for node in group {
          bytes.extend_from_slice(&node.identity.0);
        }
        Node {
          identity: ContentId::named(Form::List, &bytes),
          bytes: NodeBytes::List {
            bytes,
            first: at.saturating_mul(fanout.get()),
          },
        }
      })
      .collect()
  }

  /// The root: the top level's one node.
  fn root(&self) -> Option<&Node<'b>> {
    self.levels.last().and_then(|top| top.first())
  }

  /// The nodes a list at `level` names: `(level - 1, position)` for each identity, in order.
  fn children(level: usize, node: &Node<'_>) -> impl Iterator<Item = (usize, usize)> + use<> {
    let below = level.checked_sub(1);
    let span = match (&node.bytes, below) {
      (NodeBytes::List { bytes, first }, Some(_)) => {
        let count = bytes.len().checked_div(IDENTITY_BYTES).unwrap_or(0);
        *first..first.saturating_add(count)
      }
      _ => 0..0,
    };
    span.map(move |position| (below.unwrap_or(0), position))
  }
}

impl ContentStore {
  /// An empty store whose budget is `budget` bytes and whose window, the longest content it holds, is `window` bytes:
  /// the content plane's chunk the owner derived (module docs). Refused when the window cannot hold a list of two
  /// identities.
  pub fn new(budget: u64, window: usize) -> Result<ContentStore, ContentRefused> {
    let too_small = || ContentRefused::WindowTooSmall {
      window: u64::try_from(window).unwrap_or(u64::MAX),
    };
    let fanout = window
      .checked_div(IDENTITY_BYTES)
      .filter(|fanout| *fanout >= 2)
      .and_then(NonZeroUsize::new)
      .ok_or_else(too_small)?;
    let window = NonZeroUsize::new(window).ok_or_else(too_small)?;
    Ok(ContentStore {
      entries: BTreeMap::new(),
      window,
      fanout,
      budget,
      content_bytes: 0,
      charged_bytes: 0,
      counters: ContentCounters::default(),
    })
  }

  /// The window: the longest content the store holds.
  pub fn window(&self) -> usize {
    self.window.get()
  }

  /// Sets the budget the caller derived again. Nothing held is dropped; past it, new bytes are refused.
  pub fn set_budget(&mut self, budget: u64) {
    self.budget = budget;
  }

  /// The budget.
  pub fn budget(&self) -> u64 {
    self.budget
  }

  /// The bytes counted against the budget: every content's, and every charge's.
  pub fn used(&self) -> u64 {
    self.content_bytes.saturating_add(self.charged_bytes)
  }

  /// The bytes the budget has left.
  pub fn room(&self) -> u64 {
    self.budget.saturating_sub(self.used())
  }

  /// Refuses `need` more bytes unless the budget has room for them once `freed` charged bytes are given back.
  fn admit(&self, need: u64, freed: u64) -> Result<(), ContentRefused> {
    let room = self
      .budget
      .saturating_sub(self.used().saturating_sub(freed));
    if need > room {
      return Err(ContentRefused::OverBudget { need, room });
    }
    Ok(())
  }

  /// Refuses bytes longer than one window.
  fn within_window(&self, len: usize) -> Result<(), ContentRefused> {
    if len > self.window.get() {
      return Err(ContentRefused::PastWindow {
        len: width(len)?,
        window: width(self.window.get())?,
      });
    }
    Ok(())
  }

  /// Keeps the window `bytes`, or counts one more holder of the content already kept with their identity; the
  /// identity, held once more by the caller either way. Bytes longer than a window are refused
  /// ([`ContentRefused::PastWindow`]): an object that long is put through [`ContentStore::put_object`].
  pub fn put(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
    self.within_window(bytes.len())?;
    self.keep(Form::Window, bytes).map(|stored| stored.identity)
  }

  /// Keeps `bytes` as a content of `form`, or holds the one kept already, within the budget.
  fn keep(&mut self, form: Form, bytes: &[u8]) -> Result<Stored, ContentRefused> {
    let identity = ContentId::named(form, bytes);
    if self.entries.contains_key(&identity) {
      self.hold(&identity)?;
      self.counters.deduplicated = self.counters.deduplicated.saturating_add(1);
      return Ok(Stored {
        identity,
        fresh: false,
      });
    }
    let need = width(bytes.len())?;
    self.admit(need, 0)?;
    self.insert_new(identity, form, bytes)?;
    Ok(Stored {
      identity,
      fresh: true,
    })
  }

  /// Inserts a content the store does not hold, its first holder the caller, its bytes already admitted.
  fn insert_new(
    &mut self,
    identity: ContentId,
    form: Form,
    bytes: &[u8],
  ) -> Result<(), ContentRefused> {
    let content_bytes = self
      .content_bytes
      .checked_add(width(bytes.len())?)
      .ok_or(ContentRefused::CountExhausted)?;
    self.entries.insert(
      identity,
      Held {
        bytes: Box::from(bytes),
        form,
        holders: 1,
      },
    );
    self.content_bytes = content_bytes;
    self.counters.stored = self.counters.stored.saturating_add(1);
    if form == Form::List {
      self.counters.lists = self.counters.lists.saturating_add(1);
    }
    Ok(())
  }

  /// The root identity an object of `bytes` is held under: the plain BLAKE3 of an object of at most one window, else
  /// its top list's (module docs). Nothing is stored.
  pub fn object_id(&self, bytes: &[u8]) -> ContentId {
    if bytes.len() <= self.window.get() {
      return ContentId::of(bytes);
    }
    Tree::plan(bytes, self.window, self.fanout)
      .root()
      .map_or_else(|| ContentId::of(bytes), |root| root.identity)
  }

  /// Keeps an object's raw bytes as its windows and the lists naming them (module docs), every node the store holds
  /// already held once more instead of stored again; the root, held once more by the caller. The whole tree is admitted
  /// before any of it is kept: refused, nothing changed.
  pub fn put_object(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
    if bytes.len() <= self.window.get() {
      return self.put(bytes);
    }
    let tree = Tree::plan(bytes, self.window, self.fanout);
    let top = tree.levels.len().saturating_sub(1);
    let root = tree.root().ok_or(ContentRefused::NotHeld)?.identity;
    let need = self.new_bytes(&tree, top)?;
    self.admit(need, 0)?;
    self.keep_tree(&tree, top)?;
    Ok(root)
  }

  /// The bytes of the nodes of `tree` that the store holds none of, reached from the root through new nodes alone, each
  /// identity counted once.
  fn new_bytes(&self, tree: &Tree<'_>, top: usize) -> Result<u64, ContentRefused> {
    let mut counted: BTreeSet<ContentId> = BTreeSet::new();
    let mut need: u64 = 0;
    let mut walk = vec![(top, 0_usize)];
    while let Some((level, position)) = walk.pop() {
      let Some(node) = tree.levels.get(level).and_then(|nodes| nodes.get(position)) else {
        continue;
      };
      if self.entries.contains_key(&node.identity) || !counted.insert(node.identity) {
        continue;
      }
      need = need
        .checked_add(width(node.bytes().len())?)
        .ok_or(ContentRefused::CountExhausted)?;
      walk.extend(Tree::children(level, node));
    }
    Ok(need)
  }

  /// Keeps the admitted tree: a node held already is held once more by its parent's naming (the caller's, for the root);
  /// a new one is stored with that one holder, and a new list's nodes are kept in turn.
  fn keep_tree(&mut self, tree: &Tree<'_>, top: usize) -> Result<(), ContentRefused> {
    let mut walk = vec![(top, 0_usize)];
    while let Some((level, position)) = walk.pop() {
      let Some(node) = tree.levels.get(level).and_then(|nodes| nodes.get(position)) else {
        continue;
      };
      if self.entries.contains_key(&node.identity) {
        self.hold(&node.identity)?;
        self.counters.deduplicated = self.counters.deduplicated.saturating_add(1);
        continue;
      }
      self.insert_new(node.identity, node.form(), node.bytes())?;
      walk.extend(Tree::children(level, node));
    }
    Ok(())
  }

  /// Keeps an index's tables ([`crate::index`]) as one step: each is held once more, stored new or found held already,
  /// in place of `discharged` bytes a structure beside the store charged and now gives back, and with `charged` bytes
  /// it charges now. Every table is checked against the window and the whole step against the budget before anything
  /// changes: refused, nothing changed.
  pub fn put_tables(
    &mut self,
    tables: &[&[u8]],
    discharged: u64,
    charged: u64,
  ) -> Result<Vec<Stored>, ContentRefused> {
    let left_charged = self
      .charged_bytes
      .checked_sub(discharged)
      .ok_or(ContentRefused::NotCharged)?;
    let mut counted: BTreeSet<ContentId> = BTreeSet::new();
    let mut need = charged;
    for table in tables {
      self.within_window(table.len())?;
      let identity = ContentId::named(Form::Table, table);
      if let Some(held) = self.entries.get(&identity) {
        held
          .holders
          .checked_add(1)
          .ok_or(ContentRefused::CountExhausted)?;
      } else if counted.insert(identity) {
        need = need
          .checked_add(width(table.len())?)
          .ok_or(ContentRefused::CountExhausted)?;
      }
    }
    self.admit(need, discharged)?;
    self.charged_bytes = left_charged
      .checked_add(charged)
      .ok_or(ContentRefused::CountExhausted)?;
    let mut stored = Vec::with_capacity(tables.len());
    for table in tables {
      stored.push(self.keep_admitted(Form::Table, table)?);
    }
    Ok(stored)
  }

  /// [`ContentStore::keep`] for bytes whose room was admitted already.
  fn keep_admitted(&mut self, form: Form, bytes: &[u8]) -> Result<Stored, ContentRefused> {
    let identity = ContentId::named(form, bytes);
    if self.entries.contains_key(&identity) {
      self.hold(&identity)?;
      self.counters.deduplicated = self.counters.deduplicated.saturating_add(1);
      return Ok(Stored {
        identity,
        fresh: false,
      });
    }
    self.insert_new(identity, form, bytes)?;
    Ok(Stored {
      identity,
      fresh: true,
    })
  }

  /// Counts one more holder of the content `identity` names.
  pub fn hold(&mut self, identity: &ContentId) -> Result<(), ContentRefused> {
    let held = self
      .entries
      .get_mut(identity)
      .ok_or(ContentRefused::NotHeld)?;
    held.holders = held
      .holders
      .checked_add(1)
      .ok_or(ContentRefused::CountExhausted)?;
    Ok(())
  }

  /// Gives back one holder's count of `identity`; the content's bytes when that was its last holder, which frees it. A
  /// freed list gives back what it named, and a list freed by that what it named, a level at a time; past a refusal
  /// there the rest is given back still, and the first refusal is the answer.
  pub fn release(&mut self, identity: &ContentId) -> Result<Option<Box<[u8]>>, ContentRefused> {
    let Some((bytes, form)) = self.release_one(identity)? else {
      return Ok(None);
    };
    let mut first = Ok(());
    if form == Form::List {
      let mut named: Vec<ContentId> = bytes
        .chunks_exact(IDENTITY_BYTES)
        .filter_map(ContentId::from_slice)
        .collect();
      while let Some(next) = named.pop() {
        match self.release_one(&next) {
          Ok(Some((freed, Form::List))) => named.extend(
            freed
              .chunks_exact(IDENTITY_BYTES)
              .filter_map(ContentId::from_slice),
          ),
          Ok(_) => {}
          Err(refused) => first = first.and(Err(refused)),
        }
      }
    }
    first.map(|()| Some(bytes))
  }

  /// One count of `identity` given back; its bytes and form when it was the last.
  fn release_one(
    &mut self,
    identity: &ContentId,
  ) -> Result<Option<(Box<[u8]>, Form)>, ContentRefused> {
    let held = self
      .entries
      .get_mut(identity)
      .ok_or(ContentRefused::NotHeld)?;
    if held.holders > 1 {
      held.holders = held.holders.saturating_sub(1);
      return Ok(None);
    }
    let Some(freed) = self.entries.remove(identity) else {
      return Err(ContentRefused::NotHeld);
    };
    let len = width(freed.bytes.len())?;
    self.content_bytes = self.content_bytes.saturating_sub(len);
    self.counters.freed = self.counters.freed.saturating_add(1);
    Ok(Some((freed.bytes, freed.form)))
  }

  /// The bytes of the content `identity` names: a window's, a list's identities, or a table's.
  pub fn get(&self, identity: &ContentId) -> Option<&[u8]> {
    self.entries.get(identity).map(|held| &*held.bytes)
  }

  /// The form of the content `identity` names.
  pub fn form(&self, identity: &ContentId) -> Option<Form> {
    self.entries.get(identity).map(|held| held.form)
  }

  /// An object's windows in order, read from its root ([`ContentStore::put_object`]). A tree the store no longer holds
  /// whole ends the walk early, so a reader holds the object's length (its row's) against what it read.
  pub fn windows(&self, root: &ContentId) -> Windows<'_> {
    Windows {
      store: self,
      stack: Vec::new(),
      root: Some(*root),
    }
  }

  /// An object's bytes, read whole from its root, or `None` when the store does not hold its tree whole.
  pub fn object_bytes(&self, root: &ContentId, len: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for window in self.windows(root) {
      bytes.extend_from_slice(window);
    }
    (u64::try_from(bytes.len()).ok()? == len).then_some(bytes)
  }

  /// Whether the store holds the content `identity` names.
  pub fn contains(&self, identity: &ContentId) -> bool {
    self.entries.contains_key(identity)
  }

  /// The holders of the content `identity` names: zero when the store holds none.
  pub fn holders(&self, identity: &ContentId) -> u64 {
    self.entries.get(identity).map_or(0, |held| held.holders)
  }

  /// The contents held.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether the store holds no content.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Every content's identity, in order.
  pub fn identities(&self) -> impl Iterator<Item = &ContentId> + '_ {
    self.entries.keys()
  }

  /// Counts `bytes` a structure beside the store keeps against the budget.
  pub fn charge(&mut self, bytes: u64) -> Result<(), ContentRefused> {
    self.admit(bytes, 0)?;
    self.charged_bytes = self
      .charged_bytes
      .checked_add(bytes)
      .ok_or(ContentRefused::CountExhausted)?;
    Ok(())
  }

  /// Gives back `bytes` an earlier [`ContentStore::charge`] counted.
  pub fn discharge(&mut self, bytes: u64) -> Result<(), ContentRefused> {
    self.charged_bytes = self
      .charged_bytes
      .checked_sub(bytes)
      .ok_or(ContentRefused::NotCharged)?;
    Ok(())
  }

  /// The bytes charged now.
  pub fn charged(&self) -> u64 {
    self.charged_bytes
  }

  /// What the store has done.
  pub fn counters(&self) -> ContentCounters {
    self.counters
  }
}

/// An object's windows in order ([`ContentStore::windows`]).
#[derive(Debug)]
pub struct Windows<'s> {
  store: &'s ContentStore,
  /// The lists being read, each with the offset of the next identity it names.
  stack: Vec<(&'s [u8], usize)>,
  /// The root, until it is read.
  root: Option<ContentId>,
}

impl Windows<'_> {
  /// The next identity to read: the root first, then the top list's next one; `None` once every list is read.
  fn next_identity(&mut self) -> Option<ContentId> {
    if let Some(root) = self.root.take() {
      return Some(root);
    }
    loop {
      let (list, at) = self.stack.last_mut()?;
      let end = at.checked_add(IDENTITY_BYTES)?;
      if let Some(identity) = list.get(*at..end).and_then(ContentId::from_slice) {
        *at = end;
        return Some(identity);
      }
      self.stack.pop();
    }
  }
}

impl<'s> Iterator for Windows<'s> {
  type Item = &'s [u8];

  fn next(&mut self) -> Option<&'s [u8]> {
    loop {
      let identity = self.next_identity()?;
      let held = self.store.entries.get(&identity)?;
      if held.form != Form::List {
        return Some(&*held.bytes);
      }
      self.stack.push((&*held.bytes, 0));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The window the store tests cut at: two identities, so every list names at most two and trees grow tall quickly.
  const NARROW: usize = 2 * IDENTITY_BYTES;

  /// Three puts of `bytes` and one of other bytes: the repeated bytes one content, held three times, stored once.
  fn put_three_times(store: &mut ContentStore, bytes: &[u8]) -> ContentId {
    let first = store.put(bytes).unwrap();
    let again = store.put(bytes).unwrap();
    let third = store.put(bytes).unwrap();
    let other = store.put(b"other").unwrap();
    assert_eq!((first, first), (again, third));
    assert_ne!(first, other);
    assert_eq!((store.len(), store.holders(&first)), (2, 3));
    assert_eq!(store.used(), 13 + 5);
    assert_eq!(
      (store.counters().deduplicated, store.counters().stored),
      (2, 2)
    );
    first
  }

  /// A-581, §4.17.1 "Objects" item 1. Do: put the same bytes three times and other bytes once, then release the first
  /// three times. Expect: one content for the repeated bytes with three holders and its bytes counted once, two
  /// deduplicated puts counted, and the content freed only with its third release, its bytes handed back then.
  #[test]
  fn the_same_bytes_put_three_times_are_one_content_freed_with_its_last_holder() {
    let mut store = ContentStore::new(1 << 20, 1 << 10).unwrap();
    let bytes: &[u8] = b"fn main() {}\n";
    let first = put_three_times(&mut store, bytes);
    assert_eq!(store.release(&first).unwrap(), None);
    assert_eq!(store.release(&first).unwrap(), None);
    assert_eq!(store.get(&first), Some(bytes));
    assert_eq!(store.release(&first).unwrap().as_deref(), Some(bytes));
    assert!(!store.contains(&first));
    assert_eq!(store.used(), 5);
    assert_eq!(store.release(&first), Err(ContentRefused::NotHeld));
  }

  /// A full budget: eight bytes held twice and two charged, nothing left.
  fn full_store() -> (ContentStore, ContentId) {
    let mut store = ContentStore::new(10, NARROW).unwrap();
    let eight = store.put(b"12345678").unwrap();
    let past = Err(ContentRefused::OverBudget { need: 3, room: 2 });
    assert_eq!(store.put(b"abc").map(drop), past);
    assert_eq!(store.charge(3), past);
    assert_eq!((store.len(), store.used()), (1, 8));
    assert_eq!(store.put(b"12345678"), Ok(eight));
    assert_eq!(store.holders(&eight), 2);
    store.charge(2).unwrap();
    assert_eq!(store.room(), 0);
    (store, eight)
  }

  /// A-581. Do: fill a budget, then put past it, charge past it, and lower it under what is held. Expect: each refusal
  /// names the need and the room and changes nothing; a repeated put still counts a holder (it needs no bytes); a
  /// lowered budget drops nothing and refuses new bytes until a release makes room.
  #[test]
  fn a_put_or_a_charge_past_the_budget_is_refused_and_changes_nothing() {
    let (mut store, eight) = full_store();
    store.set_budget(4);
    assert_eq!(store.get(&eight), Some(&b"12345678"[..]));
    assert_eq!(
      store.put(b"x"),
      Err(ContentRefused::OverBudget { need: 1, room: 0 })
    );
    store.discharge(2).unwrap();
    assert_eq!(store.discharge(1), Err(ContentRefused::NotCharged));
    store.release(&eight).unwrap();
    store.release(&eight).unwrap();
    assert_eq!(store.used(), 0);
    assert!(store.put(b"x").is_ok());
  }

  /// Five windows of `NARROW` bytes, the first and the third equal, and a short sixth: an object two levels of lists
  /// tall under the narrow window.
  fn six_windows() -> Vec<u8> {
    let mut bytes = Vec::new();
    for fill in [b'a', b'b', b'a', b'c', b'd'] {
      bytes.extend(std::iter::repeat_n(fill, NARROW));
    }
    bytes.extend_from_slice(b"tail");
    bytes
  }

  /// An object of `six_windows` put and checked: its windows named by plain BLAKE3, the repeated one held twice by its
  /// list, three levels of lists, nothing longer than the window, its bytes read back. Its root.
  fn put_six_windows(store: &mut ContentStore, bytes: &[u8]) -> ContentId {
    let root = store.put_object(bytes).unwrap();
    assert_eq!(root, store.object_id(bytes));
    assert_ne!(root, ContentId::of(bytes));
    assert_eq!(store.form(&root), Some(Form::List));
    let windows: Vec<&[u8]> = bytes.chunks(NARROW).collect();
    assert_eq!(store.holders(&ContentId::of(windows[0])), 2);
    assert!(
      windows
        .iter()
        .all(|window| store.form(&ContentId::of(window)) == Some(Form::Window))
    );
    // Five distinct windows, three lists over six identities, two over three, one root.
    assert_eq!((store.len(), store.counters().lists), (5 + 3 + 2 + 1, 6));
    assert!(
      store
        .identities()
        .all(|id| store.get(id).unwrap().len() <= NARROW)
    );
    let len = u64::try_from(bytes.len()).unwrap();
    assert_eq!(store.object_bytes(&root, len).as_deref(), Some(bytes));
    root
  }

  /// A-581, §4.17.1 "Objects" item 1, D-6. Do: put an object of six windows, the first and third equal, under a window
  /// of two identities; read it back; put one of its windows alone; release everything. Expect: its windows are the
  /// plain BLAKE3 of each window of its bytes, cut at `0, w, 2w, …`, the repeated window kept once and held twice by its
  /// list; three levels of lists up to one root, which is no window's identity; the bytes read back whole, and not under
  /// another length; the window put alone is the object's own content (deduplicated, held once more); no content longer
  /// than the window; and an empty store once the object and the window are released.
  #[test]
  fn an_object_longer_than_a_window_is_held_as_its_windows_once_each() {
    let mut store = ContentStore::new(1 << 20, NARROW).unwrap();
    let bytes = six_windows();
    let root = put_six_windows(&mut store, &bytes);
    let len = u64::try_from(bytes.len()).unwrap();
    assert_eq!(store.object_bytes(&root, len + 1), None);
    let fourth = &bytes[3 * NARROW..4 * NARROW];
    let deduplicated = store.counters().deduplicated;
    assert_eq!(store.put(fourth).unwrap(), ContentId::of(fourth));
    assert_eq!(store.counters().deduplicated, deduplicated + 1);
    assert_eq!(store.holders(&ContentId::of(fourth)), 2);
    store.release(&root).unwrap();
    assert_eq!(store.len(), 1);
    assert_eq!(store.get(&ContentId::of(fourth)), Some(fourth));
    store.release(&ContentId::of(fourth)).unwrap();
    assert!(store.is_empty() && store.used() == 0);
  }

  /// A-581, §4.2. Do: put a long object twice, then a long object into a budget one byte short of its tree, and bytes
  /// past the window through `put`, and make a store whose window holds no list of two. Expect: the second put of the
  /// object holds its root once more and nothing else; the short budget refuses the whole tree with nothing stored;
  /// `put` refuses bytes past the window by kind; the narrow window is refused by kind.
  #[test]
  fn an_objects_tree_is_admitted_whole_or_not_at_all() {
    let bytes = six_windows();
    let mut roomy = ContentStore::new(1 << 20, NARROW).unwrap();
    let root = roomy.put_object(&bytes).unwrap();
    let (len, used) = (roomy.len(), roomy.used());
    assert_eq!(roomy.put_object(&bytes).unwrap(), root);
    assert_eq!(
      (roomy.len(), roomy.used(), roomy.holders(&root)),
      (len, used, 2)
    );
    let mut short = ContentStore::new(used - 1, NARROW).unwrap();
    assert_eq!(
      short.put_object(&bytes),
      Err(ContentRefused::OverBudget {
        need: used,
        room: used - 1
      })
    );
    assert!(short.is_empty());
    assert_eq!(
      short.put(&bytes[..NARROW + 1]),
      Err(ContentRefused::PastWindow {
        len: u64::try_from(NARROW + 1).unwrap(),
        window: u64::try_from(NARROW).unwrap()
      })
    );
    assert_eq!(
      ContentStore::new(1, NARROW - 1).unwrap_err(),
      ContentRefused::WindowTooSmall {
        window: u64::try_from(NARROW - 1).unwrap()
      }
    );
  }

  /// A-581. Do: put, as an object, the very bytes of a list the store holds, and as a table the same bytes. Expect:
  /// neither is taken for the list: each has an identity of its own, the window reads back as the bytes sent, and
  /// releasing it frees it alone, leaving the list's object whole.
  #[test]
  fn bytes_that_spell_a_list_or_a_table_are_a_content_of_their_own() {
    let mut store = ContentStore::new(1 << 20, NARROW).unwrap();
    let bytes = six_windows();
    let root = store.put_object(&bytes).unwrap();
    let list = store.get(&root).unwrap().to_vec();
    let spelled = store.put(&list).unwrap();
    assert_ne!(spelled, root);
    assert_eq!(store.form(&spelled), Some(Form::Window));
    assert_eq!(
      store.object_bytes(&spelled, 64).as_deref(),
      Some(list.as_slice())
    );
    let table = store.put_tables(&[list.as_slice()], 0, 0).unwrap();
    assert!(table[0].fresh && table[0].identity != spelled && table[0].identity != root);
    store.release(&spelled).unwrap();
    let len = u64::try_from(bytes.len()).unwrap();
    assert_eq!(store.object_bytes(&root, len), Some(bytes));
    store.release(&table[0].identity).unwrap();
    store.release(&root).unwrap();
    assert!(store.is_empty());
  }

  /// A-581. Do: put three tables in place of a charge, one of them twice in the batch, then put a batch whose room is
  /// one byte short, a batch with a table past the window, and a discharge past the charge. Expect: the first batch
  /// stores two contents, holds the repeated table twice and swaps the charge; each refused batch changes nothing.
  #[test]
  fn tables_are_put_as_one_step_in_place_of_a_charge() {
    let mut store = ContentStore::new(100, NARROW).unwrap();
    store.charge(40).unwrap();
    let (a, b) = (vec![1_u8; 20], vec![2_u8; 30]);
    let stored = store
      .put_tables(&[a.as_slice(), b.as_slice(), a.as_slice()], 40, 5)
      .unwrap();
    assert_eq!(
      stored.iter().map(|put| put.fresh).collect::<Vec<_>>(),
      vec![true, true, false]
    );
    assert_eq!(store.holders(&stored[0].identity), 2);
    assert_eq!((store.used(), store.charged()), (55, 5));
    let before = (store.len(), store.used(), store.charged());
    assert_eq!(
      store.put_tables(&[&[3_u8; 41]], 0, 5),
      Err(ContentRefused::OverBudget { need: 46, room: 45 })
    );
    assert!(matches!(
      store.put_tables(&[&[4_u8; NARROW + 1]], 0, 0),
      Err(ContentRefused::PastWindow { .. })
    ));
    assert_eq!(store.put_tables(&[], 6, 0), Err(ContentRefused::NotCharged));
    assert_eq!((store.len(), store.used(), store.charged()), before);
  }
}
