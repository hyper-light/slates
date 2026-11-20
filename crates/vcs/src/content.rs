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
//! **The identity rule** (main's ruling, 2026-11-20; R8: a fleet never needs uniform page sizes). An object's identity,
//! what a put answers and every caller names it by, is the plain BLAKE3 of its bytes, whatever window cut it: the window
//! is sixteen of this node's base pages, 256 KiB on macOS arm64 and 64 KiB on 4 KiB Linux, so a root differs between
//! nodes and must never cross one. The store keeps a node-local name from the identity of each object longer than a
//! window to its root ([`ContentStore::put_object`], [`ContentStore::finish_object`]), every read, hold and release by an
//! identity resolves it, and a freed root takes its name with it. A record, an entry, a homed window and a dedup key name
//! the identity; each node maps it to its own tree.
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
//! **Derived nodes** ([`Form::Derived`], A-647). Derived data ([`crate::derived`]) is kept as trees of nodes, each the
//! identities of the contents it names (files' contents, other nodes) and a payload of its type's. A node holds what it
//! names, and freeing it gives that back, as a list's does; so a value lives exactly as long as something holds its
//! root, and two values that share a subtree share its nodes. Its identity is derived under a context of its own, so no
//! bytes a client sends can be taken for a node, whose freeing would give back contents it never held.
//!
//! **Within a budget.** The store counts the bytes it keeps, and the bytes structures beside it charge to it (an
//! index's tail and its runs' fences, [`ContentStore::charge`]), against a budget the caller derives from its shard's
//! memory (§4.2) and may derive again as pressure moves ([`ContentStore::set_budget`]). A put or a charge past it is
//! refused, [`ContentRefused::OverBudget`], with nothing changed; an object's whole tree, and an index's tables, are
//! admitted as one step before a byte of them is kept. A lowered budget refuses new bytes until releases bring the
//! store under it, and never drops what is held.
//!
//! The map is ordered (`BTreeMap`) so that every walk of it is deterministic.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::num::NonZeroUsize;

/// Format: a content identity's bytes, BLAKE3's output length.
pub const IDENTITY_BYTES: usize = blake3::OUT_LEN;

/// Format: the context a list's identity is derived under ([`Form::List`]), in the form the `blake3` crate's
/// `derive_key` documents: "\[application\] \[commit timestamp\] \[purpose\]".
const LIST_CONTEXT: &str =
  "slates-vcs 2026-11-19 object store: the identities of an object's windows";

/// Format: the context a table's identity is derived under ([`Form::Table`]).
const TABLE_CONTEXT: &str = "slates-vcs 2026-11-19 object store: an index table";

/// Format: the context a derived value's node's identity is derived under ([`Form::Derived`], A-647).
const DERIVED_CONTEXT: &str = "slates-vcs 2026-11-19 object store: a derived value's node";

/// Format: the bytes of a derived node's count of the contents it names, little-endian ([`DerivedNode`]).
pub const NAMES_COUNT_BYTES: usize = size_of::<u32>();

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
  /// A derived value's node ([`crate::derived`], A-647): the contents it names, then its type's payload
  /// ([`DerivedNode`]). It holds each content it names, and freeing it gives them back.
  Derived,
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
      Form::Derived => DERIVED_CONTEXT,
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
  /// A derived node may not name `identity`, an index's table: a table's rows are given back by the index that frees
  /// it ([`crate::index`]), which a node's freeing could not do.
  NotNameable {
    /// The table named.
    identity: ContentId,
  },
  /// The storage the windows are kept in ([`WindowStorage`], A-727) refused to keep a window: its room, or the arena
  /// under it.
  Storage,
}

/// A window kept by a [`WindowStorage`]: the handle the storage names it by, opaque to the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StoredWindow(pub u64);

/// Where a content store keeps its windows' bytes (A-727: repository bytes leave the partition snapshot). The store
/// keeps every window through it and its own metadata (lists, tables, derived nodes) in its own memory. The daemon's
/// storage keeps each window as a sealed chunk of the shard's repository arena in the anchor's content object, durable
/// there and read by copying it out (decrypted where the shard seals, A-99); [`HeapWindows`] keeps them in the
/// process's memory, for tests and tools.
pub trait WindowStorage: std::fmt::Debug {
  /// Keeps `bytes`, at most one window: the handle to read and release them by.
  ///
  /// # Errors
  /// [`ContentRefused::Storage`] when the storage cannot keep them; nothing is kept.
  fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused>;
  /// The bytes kept under `window`: borrowed where the storage can lend them, copied out where it cannot.
  fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>>;
  /// Lets the bytes kept under `window` go.
  fn release(&mut self, window: StoredWindow);

  /// Takes in `lent`, when it is what this storage keeps its windows in and none is lent to it now (A-727: the shard's
  /// chunk store, lent for one step), leaving what it held in its place: whether it took it. A storage that keeps its
  /// windows itself takes nothing.
  fn lend_in(&mut self, _lent: &mut dyn std::any::Any) -> bool {
    false
  }

  /// Hands back what [`WindowStorage::lend_in`] took, exchanging it with `lent` again: whether it held one.
  fn hand_back(&mut self, _lent: &mut dyn std::any::Any) -> bool {
    false
  }

  /// Names the window just kept under `window` by its identity (A-727 slice a3): what a storage that outlives the
  /// process keeps beside it, so a store made again after a restart takes it back by identity
  /// ([`WindowStorage::restored`]). A storage that does not outlive the process keeps nothing.
  fn named(&mut self, _window: StoredWindow, _identity: &ContentId) {}

  /// The windows this storage kept from before a restart (A-727 slice a3), each with its identity and length, handed
  /// over once to the store made over it, which holds them for no one until a holder puts them again
  /// ([`ContentStore::release_unheld`] frees the rest).
  fn restored(&mut self) -> Vec<(ContentId, StoredWindow, usize)> {
    Vec::new()
  }

  /// This storage as `Any`, for an owner that reads what its own storage type keeps (A-727 slice a3: the shard image's
  /// table of the windows); none for a storage that offers nothing.
  fn as_any(&self) -> Option<&dyn std::any::Any> {
    None
  }

  /// [`WindowStorage::as_any`], mutably.
  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    None
  }
}

/// The windows kept in the process's memory: the storage of a store made with [`ContentStore::new`], for tests and
/// tools; the daemon gives its store the shard's repository arena instead ([`ContentStore::with_storage`]).
#[derive(Debug, Default)]
pub struct HeapWindows {
  /// Each window's bytes, by handle.
  slots: BTreeMap<StoredWindow, Box<[u8]>>,
  /// The next handle.
  next: u64,
}

impl WindowStorage for HeapWindows {
  fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
    let window = StoredWindow(self.next);
    self.next = self
      .next
      .checked_add(1)
      .ok_or(ContentRefused::CountExhausted)?;
    self.slots.insert(window, Box::from(bytes));
    Ok(window)
  }

  fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
    self.slots.get(&window).map(|bytes| Cow::Borrowed(&**bytes))
  }

  fn release(&mut self, window: StoredWindow) {
    self.slots.remove(&window);
  }
}

/// Where a held content's bytes are: in the store's own memory (its lists, tables and derived nodes), or kept by its
/// window storage (its windows).
#[derive(Debug)]
enum Kept {
  /// In the store's memory.
  Here(Box<[u8]>),
  /// Kept by the window storage.
  Stored {
    /// The storage's handle.
    window: StoredWindow,
    /// The window's length.
    len: usize,
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
  /// Derived nodes stored new ([`ContentStore::put_derived`]).
  pub derived: u64,
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
  kept: Kept,
  form: Form,
  holders: u64,
}

impl Held {
  /// The bytes kept in the store's own memory: a list's, a table's or a derived node's; none for a window.
  fn here(&self) -> Option<&[u8]> {
    match &self.kept {
      Kept::Here(bytes) => Some(bytes),
      Kept::Stored { .. } => None,
    }
  }

  /// Its length in bytes.
  fn len(&self) -> usize {
    match &self.kept {
      Kept::Here(bytes) => bytes.len(),
      Kept::Stored { len, .. } => *len,
    }
  }
}

/// An object being put a window at a time ([`ContentStore::begin_object`]): the bytes short of a window, and the
/// identities of the windows (later, of the lists) kept so far, each holding one count.
#[derive(Debug, Default)]
pub struct Writing {
  /// The bytes of the window being filled.
  pending: Vec<u8>,
  /// The BLAKE3 of the object's bytes so far: its identity once finished.
  hasher: blake3::Hasher,
  /// The identities kept so far, in order.
  windows: Vec<ContentId>,
}

impl Writing {
  /// The windows kept so far.
  pub fn windows_kept(&self) -> usize {
    self.windows.len()
  }
}

/// The content store (module docs).
#[derive(Debug)]
pub struct ContentStore {
  entries: BTreeMap<ContentId, Held>,
  /// The identity rule (main's ruling, 2026-11-20; R8): each object longer than a window by its identity, the BLAKE3 of
  /// its bytes, to the root of the tree this store cut it into, a node-local detail; and back.
  objects: BTreeMap<ContentId, ContentId>,
  /// [`ContentStore::objects`] inverted: a root to the identity of the object it roots.
  rooted: BTreeMap<ContentId, ContentId>,
  /// Where the windows' bytes are kept (A-727).
  storage: Box<dyn WindowStorage>,
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

/// The storage handle of a held window, none for content in the store's memory.
fn held_window(held: &Held) -> Option<StoredWindow> {
  match held.kept {
    Kept::Stored { window, .. } => Some(window),
    Kept::Here(_) => None,
  }
}

/// What giving back a content's last count frees: its bytes and its form, or nothing while other holders remain.
type Released = Option<(Box<[u8]>, Form)>;

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
      objects: BTreeMap::new(),
      rooted: BTreeMap::new(),
      storage: Box::new(HeapWindows::default()),
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
    let kept = if form == Form::Window {
      let window = self.storage.put(bytes)?;
      self.storage.named(window, &identity);
      Kept::Stored {
        window,
        len: bytes.len(),
      }
    } else {
      Kept::Here(Box::from(bytes))
    };
    self.entries.insert(
      identity,
      Held {
        kept,
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
    ContentId::of(bytes)
  }

  /// The node an object's identity names in this store, its root (module docs, "The identity rule"): what a record
  /// written before the rule named an object longer than a window by, so a replay of one can tell it is the same
  /// object. Node-local: never sent or recorded.
  pub fn root_of(&self, identity: &ContentId) -> ContentId {
    self.resolve(identity)
  }

  /// The root this store would cut `bytes` into ([`ContentStore::root_of`] for bytes not held): what a version-1 git
  /// record, written before the identity rule, named an object longer than a window by.
  pub fn root_id(&self, bytes: &[u8]) -> ContentId {
    if bytes.len() <= self.window.get() {
      return ContentId::of(bytes);
    }
    Tree::plan(bytes, self.window, self.fanout)
      .root()
      .map_or_else(|| ContentId::of(bytes), |root| root.identity)
  }

  /// `identity`, the BLAKE3 of an object's bytes, named for `root`, the tree this store keeps it as (module docs, "The
  /// identity rule"): a root that is the identity itself (an object of at most one window) needs no name. The identity.
  fn named_object(&mut self, identity: ContentId, root: ContentId) -> ContentId {
    if identity != root {
      self.objects.insert(identity, root);
      self.rooted.insert(root, identity);
    }
    identity
  }

  /// The node an identity names in this store: an object's root for an object longer than a window, else itself.
  fn resolve(&self, identity: &ContentId) -> ContentId {
    *self.objects.get(identity).unwrap_or(identity)
  }

  /// Keeps an object's raw bytes as its windows and the lists naming them (module docs), every node the store holds
  /// already held once more instead of stored again; the root, held once more by the caller. The whole tree is admitted
  /// before any of it is kept: refused, nothing changed.
  pub fn put_object(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
    let root = self.put_object_root(bytes)?;
    Ok(self.named_object(ContentId::of(bytes), root))
  }

  /// [`ContentStore::put_object`]'s tree: its root.
  fn put_object_root(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
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

  /// Starts an object put a window at a time ([`ContentStore::put_window`], [`ContentStore::finish_object`]): what an
  /// intake streams, its bytes arriving in pieces it never holds whole (§4.17.1 "A push's intake", rule 1; Subversion's
  /// commit texts, A-727 slice c). The same object [`ContentStore::put_object`] keeps, the same root, its windows kept as
  /// each fills.
  pub fn begin_object(&self) -> Writing {
    Writing::default()
  }

  /// Takes `bytes`, the object's next piece of any length: every window it fills is kept at once (one holder, which the
  /// list naming it takes over), the rest held for the next piece, so the writer holds less than a window. Refused (the
  /// budget, the storage), what the object kept so far stays held by `writing`, for [`ContentStore::abandon_object`].
  ///
  /// # Errors
  /// [`ContentStore::put`]'s.
  pub fn put_window(&mut self, writing: &mut Writing, bytes: &[u8]) -> Result<(), ContentRefused> {
    let window = self.window.get();
    writing.hasher.update(bytes);
    let mut rest = bytes;
    while !rest.is_empty() {
      let room = window.saturating_sub(writing.pending.len());
      let (taken, left) = rest.split_at(room.min(rest.len()));
      writing.pending.extend_from_slice(taken);
      rest = left;
      if writing.pending.len() == window {
        let identity = self.put(&writing.pending)?;
        writing.windows.push(identity);
        writing.pending.clear();
      }
    }
    Ok(())
  }

  /// Ends the object: its last window kept, then the lists naming its windows, `fanout` a list, level by level, as
  /// [`ContentStore::put_object`] plans them; the root, held once more by the caller. A list the store holds already is
  /// held once more and gives back the holds its pieces took, as `put_object` holds a node found held.
  ///
  /// # Errors
  /// [`ContentStore::put`]'s; refused, what was kept stays held by `writing`'s identities, which the caller abandons.
  pub fn finish_object(&mut self, writing: &mut Writing) -> Result<ContentId, ContentRefused> {
    let identity = ContentId(*writing.hasher.finalize().as_bytes());
    let root = self.finish_root(writing)?;
    Ok(self.named_object(identity, root))
  }

  /// [`ContentStore::finish_object`]'s tree: its root.
  fn finish_root(&mut self, writing: &mut Writing) -> Result<ContentId, ContentRefused> {
    if !writing.pending.is_empty() || writing.windows.is_empty() {
      let identity = self.put(&writing.pending)?;
      writing.windows.push(identity);
      writing.pending.clear();
    }
    let mut level = std::mem::take(&mut writing.windows);
    while level.len() > 1 {
      let mut above = Vec::with_capacity(level.len().div_ceil(self.fanout.get()));
      for (at, group) in level.chunks(self.fanout.get()).enumerate() {
        let mut bytes = Vec::with_capacity(group.len().saturating_mul(IDENTITY_BYTES));
        for identity in group {
          bytes.extend_from_slice(&identity.0);
        }
        let stored = match self.keep(Form::List, &bytes) {
          Ok(stored) => stored,
          Err(refused) => {
            // The lists made so far hold their pieces; the pieces no list names yet hold themselves.
            let unlisted = level
              .get(at.saturating_mul(self.fanout.get())..)
              .unwrap_or_default();
            above.extend_from_slice(unlisted);
            writing.windows = above;
            return Err(refused);
          }
        };
        if !stored.fresh {
          for identity in group {
            self.release(identity)?;
          }
        }
        above.push(stored.identity);
      }
      level = above;
    }
    level.pop().ok_or(ContentRefused::NotHeld)
  }

  /// Gives back what an unfinished object kept: its windows' and lists' holds, its pending bytes dropped.
  pub fn abandon_object(&mut self, writing: Writing) {
    for identity in &writing.windows {
      let _ = self.release(identity);
    }
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
    let identity = &self.resolve(identity);
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

  /// Keeps a derived value's node ([`Form::Derived`], A-647): `names`, the contents it names, each of which the store
  /// must hold, then `payload`, its type's own bytes, at most one window in all ([`DerivedNode`] is the layout). Stored
  /// new, it holds each content it names once for each time it names it, so they live as long as it does, and freeing
  /// it gives them back ([`ContentStore::release`]); held already, it is held once more. Either way the caller holds it
  /// once more. A table is never named ([`ContentRefused::NotNameable`]). Refused, nothing changed.
  pub fn put_derived(
    &mut self,
    names: &[ContentId],
    payload: &[u8],
  ) -> Result<Stored, ContentRefused> {
    let bytes = encode_derived(names, payload).ok_or(ContentRefused::CountExhausted)?;
    self.within_window(bytes.len())?;
    let identity = ContentId::named(Form::Derived, &bytes);
    if self.entries.contains_key(&identity) {
      self.hold(&identity)?;
      self.counters.deduplicated = self.counters.deduplicated.saturating_add(1);
      return Ok(Stored {
        identity,
        fresh: false,
      });
    }
    self.can_name(names)?;
    self.admit(width(bytes.len())?, 0)?;
    self.insert_new(identity, Form::Derived, &bytes)?;
    for name in names {
      self.hold(name)?;
    }
    self.counters.derived = self.counters.derived.saturating_add(1);
    Ok(Stored {
      identity,
      fresh: true,
    })
  }

  /// Refuses `names` unless the store holds each, none is a table, and each count can take one more for every time it
  /// is named.
  fn can_name(&self, names: &[ContentId]) -> Result<(), ContentRefused> {
    let mut times: BTreeMap<ContentId, u64> = BTreeMap::new();
    for name in names {
      let count = times.entry(*name).or_insert(0);
      *count = count.checked_add(1).ok_or(ContentRefused::CountExhausted)?;
    }
    for (name, times) in &times {
      let held = self
        .entries
        .get(&self.resolve(name))
        .ok_or(ContentRefused::NotHeld)?;
      if held.form == Form::Table {
        return Err(ContentRefused::NotNameable { identity: *name });
      }
      held
        .holders
        .checked_add(*times)
        .ok_or(ContentRefused::CountExhausted)?;
    }
    Ok(())
  }

  /// The derived node `identity` names, read in place; `None` when the store holds no derived node of it.
  pub fn derived(&self, identity: &ContentId) -> Option<DerivedNode<'_>> {
    let identity = &self.resolve(identity);
    let held = self
      .entries
      .get(identity)
      .filter(|held| held.form == Form::Derived)?;
    DerivedNode::open(held.here()?)
  }

  /// Gives back one holder's count of `identity`; the content's bytes when that was its last holder, which frees it. A
  /// freed list gives back what it named, and a freed derived node what it names, and each list or node freed by that
  /// what it names, until nothing more is freed; past a refusal there the rest is given back still, and the first
  /// refusal is the answer. The cascade is one call, as long as what it frees (slicing it is owed with A-581's).
  pub fn release(&mut self, identity: &ContentId) -> Result<Option<Box<[u8]>>, ContentRefused> {
    let identity = &self.resolve(identity);
    let Some((bytes, form)) = self.release_one(identity)? else {
      return Ok(None);
    };
    let mut named: Vec<ContentId> = Vec::new();
    given_back(form, &bytes, &mut named);
    let mut first = Ok(());
    while let Some(next) = named.pop() {
      // A derived node names an object by its identity; a list names its own windows and lists.
      let next = self.resolve(&next);
      match self.release_one(&next) {
        Ok(Some((freed, form))) => given_back(form, &freed, &mut named),
        Ok(None) => {}
        Err(refused) => first = first.and(Err(refused)),
      }
    }
    first.map(|()| Some(bytes))
  }

  /// One count of `identity` given back; its bytes and form when it was the last.
  fn release_one(&mut self, identity: &ContentId) -> Result<Released, ContentRefused> {
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
    // A freed root takes its object's name with it (module docs, "The identity rule").
    if let Some(named) = self.rooted.remove(identity) {
      self.objects.remove(&named);
    }
    let len = width(freed.len())?;
    self.content_bytes = self.content_bytes.saturating_sub(len);
    self.counters.freed = self.counters.freed.saturating_add(1);
    let bytes = match freed.kept {
      Kept::Here(bytes) => bytes,
      Kept::Stored { window, .. } => {
        let bytes = self
          .storage
          .read(window)
          .map(|read| Box::from(&*read))
          .unwrap_or_default();
        self.storage.release(window);
        bytes
      }
    };
    Ok(Some((bytes, freed.form)))
  }

  /// The bytes of the content `identity` names: a window's, a list's identities, or a table's. Lists and tables are the
  /// store's own metadata and stay in its memory, so they are lent; an object's windows are read through
  /// [`ContentStore::windows`] and [`ContentStore::window_at`], which answer a `Cow` (A-727: a window kept as a sealed
  /// chunk of the shard's arena is copied out to be read).
  pub fn get(&self, identity: &ContentId) -> Option<&[u8]> {
    let identity = &self.resolve(identity);
    self.entries.get(identity).and_then(Held::here)
  }

  /// The bytes of the content `identity` names, whatever its form: a window's from the window storage (borrowed where
  /// it lends them, copied out where it cannot, A-727), a list's, table's or derived node's from the store's memory.
  pub fn bytes_of(&self, identity: &ContentId) -> Option<Cow<'_, [u8]>> {
    let identity = &self.resolve(identity);
    let held = self.entries.get(identity)?;
    match &held.kept {
      Kept::Here(bytes) => Some(Cow::Borrowed(bytes)),
      Kept::Stored { window, .. } => self.storage.read(*window),
    }
  }

  /// Lends `lent` to the window storage for one step (A-727): whether it took it ([`WindowStorage::lend_in`]). The
  /// lender hands it back with [`ContentStore::hand_back`] once the step ends.
  pub fn lend_in(&mut self, lent: &mut dyn std::any::Any) -> bool {
    self.storage.lend_in(lent)
  }

  /// Hands back what [`ContentStore::lend_in`] lent: whether the storage held one.
  pub fn hand_back(&mut self, lent: &mut dyn std::any::Any) -> bool {
    self.storage.hand_back(lent)
  }

  /// A store whose windows are kept by `storage` (A-727: the daemon's shard repository arena), otherwise as
  /// [`ContentStore::new`] makes one.
  ///
  /// # Errors
  /// As [`ContentStore::new`].
  pub fn with_storage(
    budget: u64,
    window: usize,
    storage: Box<dyn WindowStorage>,
  ) -> Result<ContentStore, ContentRefused> {
    let mut store = ContentStore::new(budget, window)?;
    store.storage = storage;
    // The windows the storage kept from before a restart (A-727 slice a3), held for no one until a holder puts them.
    for (identity, kept, len) in store.storage.restored() {
      let bytes = width(len)?;
      store.content_bytes = store
        .content_bytes
        .checked_add(bytes)
        .ok_or(ContentRefused::CountExhausted)?;
      store.entries.insert(
        identity,
        Held {
          kept: Kept::Stored { window: kept, len },
          form: Form::Window,
          holders: 0,
        },
      );
    }
    Ok(store)
  }

  /// Frees every window the storage kept from before a restart that no holder put again (A-727 slice a3): what the
  /// owner calls once the records are replayed, since a window no record names is held by no one. The count freed.
  pub fn release_unheld(&mut self) -> u64 {
    let unheld: Vec<ContentId> = self
      .entries
      .iter()
      .filter(|(_, held)| held.holders == 0)
      .map(|(identity, _)| *identity)
      .collect();
    let mut freed = 0u64;
    for identity in unheld {
      let Some(held) = self.entries.remove(&identity) else {
        continue;
      };
      self.content_bytes = self
        .content_bytes
        .saturating_sub(u64::try_from(held.len()).unwrap_or(u64::MAX));
      if let Kept::Stored { window, .. } = held.kept {
        self.storage.release(window);
      }
      freed = freed.saturating_add(1);
    }
    self.counters.freed = self.counters.freed.saturating_add(freed);
    freed
  }

  /// The storage the windows are kept in (A-727 slice a3: its owner images it).
  pub fn storage(&self) -> &dyn WindowStorage {
    &*self.storage
  }

  /// [`ContentStore::storage`], mutably.
  pub fn storage_mut(&mut self) -> &mut dyn WindowStorage {
    &mut *self.storage
  }

  /// The length in bytes of the content `identity` names, whatever its form, read without its bytes.
  pub fn len_of(&self, identity: &ContentId) -> Option<u64> {
    let identity = &self.resolve(identity);
    self
      .entries
      .get(identity)
      .and_then(|held| u64::try_from(held.len()).ok())
  }

  /// The form of the content `identity` names.
  pub fn form(&self, identity: &ContentId) -> Option<Form> {
    let identity = &self.resolve(identity);
    self.entries.get(identity).map(|held| held.form)
  }

  /// An object's windows in order, read from its root ([`ContentStore::put_object`]). A tree the store no longer holds
  /// whole ends the walk early, so a reader holds the object's length (its row's) against what it read.
  pub fn windows(&self, root: &ContentId) -> Windows<'_> {
    let root = &self.resolve(root);
    Windows {
      store: self,
      stack: Vec::new(),
      root: Some(*root),
    }
  }

  /// The window `index` of an object `len` bytes long whose root is `root` ([`ContentStore::put_object`]), read down
  /// the one list a level that names it (A-639): the object was cut into windows of this store's window and lists of
  /// its fanout, so the list naming window `index` at each level is that level's digit of `index` in the fanout's base.
  /// A reader that resumes at a window reads the lists above it alone, never the windows before it. `None` past the
  /// object's last window, or where the store does not hold the path down whole.
  pub fn window_at(&self, root: &ContentId, len: u64, index: u64) -> Option<Cow<'_, [u8]>> {
    let root = &self.resolve(root);
    let window = u64::try_from(self.window.get()).ok()?;
    let fanout = u64::try_from(self.fanout.get()).ok()?;
    let windows = len.div_ceil(window).max(1);
    if index >= windows {
      return None;
    }
    let height = list_levels(windows, fanout)?;
    let identity = self.window_identity(root, (height, fanout), index)?;
    let held = self
      .entries
      .get(&identity)
      .filter(|held| held.form == Form::Window)?;
    match held_window(held) {
      Some(window) => self.storage.read(window),
      None => held.here().map(Cow::Borrowed),
    }
  }

  /// The identity of window `index` under `root`, read down the one list a level of a tree `height` lists deep, each of
  /// `fanout` children: at each level the child is that level's digit of `index` in the fanout's base.
  fn window_identity(
    &self,
    root: &ContentId,
    (height, fanout): (u32, u64),
    index: u64,
  ) -> Option<ContentId> {
    let mut identity = *root;
    for level in (0..height).rev() {
      let span = fanout.checked_pow(level)?;
      let child = index.checked_div(span)?.checked_rem(fanout)?;
      let list = self
        .entries
        .get(&identity)
        .filter(|held| held.form == Form::List)?;
      let from = usize::try_from(child).ok()?.checked_mul(IDENTITY_BYTES)?;
      let to = from.checked_add(IDENTITY_BYTES)?;
      identity = list.here()?.get(from..to).and_then(ContentId::from_slice)?;
    }
    Some(identity)
  }

  /// An object's bytes, read whole from its root, or `None` when the store does not hold its tree whole.
  pub fn object_bytes(&self, root: &ContentId, len: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    for window in self.windows(root) {
      bytes.extend_from_slice(&window);
    }
    (u64::try_from(bytes.len()).ok()? == len).then_some(bytes)
  }

  /// Whether the store holds the content `identity` names.
  pub fn contains(&self, identity: &ContentId) -> bool {
    let identity = &self.resolve(identity);
    self.entries.contains_key(identity)
  }

  /// The holders of the content `identity` names: zero when the store holds none.
  pub fn holders(&self, identity: &ContentId) -> u64 {
    let identity = &self.resolve(identity);
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

/// The contents a freed content of `form` held, which freeing it gives back: a list's identities, a derived node's
/// names. A window names nothing, and a table's rows are given back by the index that freed it ([`crate::index`]).
fn given_back(form: Form, bytes: &[u8], named: &mut Vec<ContentId>) {
  match form {
    Form::List => named.extend(
      bytes
        .as_chunks::<IDENTITY_BYTES>()
        .0
        .iter()
        .copied()
        .map(ContentId),
    ),
    Form::Derived => named.extend(
      DerivedNode::open(bytes)
        .into_iter()
        .flat_map(|node| node.names()),
    ),
    Form::Window | Form::Table => {}
  }
}

/// A derived node's bytes ([`Form::Derived`]): the count of the contents it names (four bytes, little-endian), their
/// identities, then the payload. `None` when there are more names than the count holds.
pub fn encode_derived(names: &[ContentId], payload: &[u8]) -> Option<Vec<u8>> {
  let count = u32::try_from(names.len()).ok()?;
  let len = names
    .len()
    .checked_mul(IDENTITY_BYTES)?
    .checked_add(NAMES_COUNT_BYTES)?
    .checked_add(payload.len())?;
  let mut bytes = Vec::with_capacity(len);
  bytes.extend_from_slice(&count.to_le_bytes());
  for name in names {
    bytes.extend_from_slice(&name.0);
  }
  bytes.extend_from_slice(payload);
  Some(bytes)
}

/// A derived node read in place from its bytes ([`encode_derived`]): the contents it names, in order, and its type's
/// payload.
#[derive(Clone, Copy, Debug)]
pub struct DerivedNode<'a> {
  names: &'a [[u8; IDENTITY_BYTES]],
  payload: &'a [u8],
}

impl<'a> DerivedNode<'a> {
  /// The node `bytes` hold, or `None` when they are not a node's: shorter than a count, or than the names it counts.
  pub fn open(bytes: &'a [u8]) -> Option<DerivedNode<'a>> {
    let (count, rest) = bytes.split_first_chunk::<NAMES_COUNT_BYTES>()?;
    let count = usize::try_from(u32::from_le_bytes(*count)).ok()?;
    let (names, payload) = rest.split_at_checked(count.checked_mul(IDENTITY_BYTES)?)?;
    Some(DerivedNode {
      names: names.as_chunks::<IDENTITY_BYTES>().0,
      payload,
    })
  }

  /// The contents it names.
  pub fn len(&self) -> usize {
    self.names.len()
  }

  /// Whether it names no content.
  pub fn is_empty(&self) -> bool {
    self.names.is_empty()
  }

  /// The content it names at `at`.
  pub fn name(&self, at: usize) -> Option<ContentId> {
    self.names.get(at).copied().map(ContentId)
  }

  /// The contents it names, in order.
  pub fn names(&self) -> impl Iterator<Item = ContentId> + use<'a> {
    self.names.iter().copied().map(ContentId)
  }

  /// Its type's payload.
  pub fn payload(&self) -> &'a [u8] {
    self.payload
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
  type Item = Cow<'s, [u8]>;

  fn next(&mut self) -> Option<Cow<'s, [u8]>> {
    loop {
      let identity = self.next_identity()?;
      let held = self.store.entries.get(&identity)?;
      if held.form != Form::List {
        return match &held.kept {
          Kept::Here(bytes) => Some(Cow::Borrowed(bytes)),
          Kept::Stored { window, .. } => self.store.storage.read(*window),
        };
      }
      self.stack.push((held.here()?, 0));
    }
  }
}

/// How many levels of lists sit above `windows` windows, each list naming `fanout` children: none for one window.
fn list_levels(windows: u64, fanout: u64) -> Option<u32> {
  let mut height = 0_u32;
  let mut nodes = windows;
  while nodes > 1 {
    nodes = nodes.div_ceil(fanout);
    height = height.checked_add(1)?;
  }
  Some(height)
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
    assert_eq!(store.bytes_of(&first).as_deref(), Some(bytes));
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
    assert_eq!(store.bytes_of(&eight).as_deref(), Some(&b"12345678"[..]));
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
    for fill in *b"abacd" {
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
    // The identity rule: the object's identity is the BLAKE3 of its bytes; its root, a list, is the store's own.
    assert_eq!(root, ContentId::of(bytes));
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
        .all(|id| store.bytes_of(id).unwrap().len() <= NARROW)
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
    assert_eq!(
      store.bytes_of(&ContentId::of(fourth)).as_deref(),
      Some(fourth)
    );
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

  /// A-639 (a stream resumes an object at a window). Do: keep objects from empty to tens of windows long under a narrow
  /// window, whose lists of two grow tall trees, and a wider one; read each window by its index. Expect: every window
  /// the in-order walk yields, at its index; nothing past the last; nothing for an object the store never held.
  #[test]
  fn a_window_read_by_its_index_is_the_walks_window() {
    for window in [NARROW, 3 * IDENTITY_BYTES] {
      let mut store = ContentStore::new(1 << 24, window).unwrap();
      for len in [
        0,
        1,
        window - 1,
        window,
        window + 1,
        7 * window + 3,
        33 * window,
      ] {
        let bytes: Vec<u8> = (0..len).map(|at| u8::try_from(at % 251).unwrap()).collect();
        let root = store.put_object(&bytes).unwrap();
        let walked: Vec<std::borrow::Cow<'_, [u8]>> = store.windows(&root).collect();
        let whole = u64::try_from(len).unwrap();
        for (index, expected) in walked.iter().enumerate() {
          let at = u64::try_from(index).unwrap();
          assert_eq!(
            store.window_at(&root, whole, at),
            Some(expected.clone()),
            "{len} {index}"
          );
        }
        let past = u64::try_from(walked.len()).unwrap();
        assert_eq!(store.window_at(&root, whole, past), None);
      }
      let never = ContentId::of(b"never kept");
      assert_eq!(store.window_at(&never, 10, 0), None);
    }
  }
  /// A storage that never lends: every read is a copy, as the daemon's arena storage reads a sealed chunk (A-727).
  #[derive(Debug, Default)]
  struct CopyingWindows {
    inner: HeapWindows,
  }

  impl WindowStorage for CopyingWindows {
    fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
      self.inner.put(bytes)
    }

    fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
      self
        .inner
        .read(window)
        .map(|bytes| Cow::Owned(bytes.into_owned()))
    }

    fn release(&mut self, window: StoredWindow) {
      self.inner.release(window);
    }
  }

  /// A-727 slice a2 (the window storage seam). Do: in a store whose storage copies every read out, put an object of
  /// five windows twice and a one-window object, read both back whole and window by window, then release the long
  /// object's two holds. Expect: the second put answered by the first's root; the lists in the store's own memory, lent
  /// by `get`, the windows not; every window read copied out (`Cow::Owned`) and equal to the bytes put; and the long
  /// object gone with its last hold while the short one stays readable.
  #[test]
  fn windows_kept_by_a_storage_that_copies_out_read_and_free_as_the_heap_store_does() {
    let mut store =
      ContentStore::with_storage(1 << 20, NARROW, Box::new(CopyingWindows::default())).unwrap();
    let long: Vec<u8> = (0..5 * NARROW)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    let root = store.put_object(&long).unwrap();
    assert_eq!(store.put_object(&long).unwrap(), root);
    let short = store.put_object(b"short").unwrap();
    let len = u64::try_from(long.len()).unwrap();
    assert_eq!(store.object_bytes(&root, len), Some(long.clone()));
    assert!(
      store
        .windows(&root)
        .all(|window| matches!(window, Cow::Owned(_)))
    );
    assert!(
      store.get(&root).is_some(),
      "the long object's root is a list, lent from the store's memory"
    );
    assert!(
      store.get(&short).is_none(),
      "a window is kept by the storage, not lent by get"
    );
    assert_eq!(store.bytes_of(&short).as_deref(), Some(&b"short"[..]));
    store.release(&root).unwrap();
    assert_eq!(store.object_bytes(&root, len), Some(long));
    store.release(&root).unwrap();
    assert_eq!(store.object_bytes(&root, len), None);
    assert_eq!(store.bytes_of(&short).as_deref(), Some(&b"short"[..]));
  }

  /// A storage that kept windows from before a restart (A-727 slice a3): the windows `kept` names, by identity, handed
  /// to the store made over it.
  #[derive(Debug, Default)]
  struct RestoredWindows {
    inner: HeapWindows,
    kept: Vec<(ContentId, StoredWindow, usize)>,
    named: Vec<(StoredWindow, ContentId)>,
  }

  impl WindowStorage for RestoredWindows {
    fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
      self.inner.put(bytes)
    }

    fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
      self.inner.read(window)
    }

    fn release(&mut self, window: StoredWindow) {
      self.inner.release(window);
    }

    fn named(&mut self, window: StoredWindow, identity: &ContentId) {
      self.named.push((window, *identity));
    }

    fn restored(&mut self) -> Vec<(ContentId, StoredWindow, usize)> {
      std::mem::take(&mut self.kept)
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
      Some(self)
    }
  }

  /// A-727 slice a3 (windows kept across a restart). Do: give a storage three windows it kept before a restart; make a
  /// store over it; put one of them again and a new one; free the windows no holder put again. Expect: the three held
  /// for no one and counted in what the store holds, readable by identity; the put again answered by the window kept,
  /// stored nothing new and held once; the new one stored and named to the storage by its identity; then the two no
  /// one put freed, released from the storage, and the store holding the two that are held.
  #[test]
  fn windows_kept_from_before_a_restart_are_held_again_by_a_put_and_the_rest_freed() {
    let mut storage = RestoredWindows::default();
    let windows: [&[u8]; 3] = [
      b"kept and put again",
      b"kept, never named again",
      b"kept too",
    ];
    for bytes in windows {
      let window = storage.inner.put(bytes).unwrap();
      storage
        .kept
        .push((ContentId::of(bytes), window, bytes.len()));
    }
    let mut store = ContentStore::with_storage(1 << 20, NARROW, Box::new(storage)).unwrap();
    assert_kept_held_for_no_one(&store, windows);
    let stored = store.counters().stored;
    assert_eq!(store.put(windows[0]).unwrap(), ContentId::of(windows[0]));
    assert_eq!(
      store.counters().stored,
      stored,
      "the window kept answers the put"
    );
    assert_eq!(store.holders(&ContentId::of(windows[0])), 1);
    let fresh = store.put(b"new since the restart").unwrap();
    assert_eq!(store.release_unheld(), 2);
    assert_eq!(store.len(), 2);
    assert_eq!(store.bytes_of(&ContentId::of(windows[1])), None);
    assert_eq!(
      store.bytes_of(&ContentId::of(windows[0])).as_deref(),
      Some(windows[0])
    );
    assert_eq!(
      store.bytes_of(&fresh).as_deref(),
      Some(&b"new since the restart"[..])
    );
    assert_only_the_new_window_named(&store, fresh);
  }

  /// The windows kept before the restart, held for no one, counted in what `store` holds and readable by identity.
  fn assert_kept_held_for_no_one(store: &ContentStore, windows: [&[u8]; 3]) {
    let held: u64 = windows
      .iter()
      .map(|bytes| u64::try_from(bytes.len()).unwrap())
      .sum();
    assert_eq!((store.len(), store.used()), (3, held));
    for bytes in windows {
      assert_eq!(store.holders(&ContentId::of(bytes)), 0);
      assert_eq!(
        store.bytes_of(&ContentId::of(bytes)).as_deref(),
        Some(bytes)
      );
    }
  }

  /// Only `fresh`, the window stored since the restart, was named to the storage, by its identity.
  fn assert_only_the_new_window_named(store: &ContentStore, fresh: ContentId) {
    let named = store
      .storage()
      .as_any()
      .and_then(|any| any.downcast_ref::<RestoredWindows>())
      .map(|storage| {
        storage
          .named
          .iter()
          .map(|(_, identity)| *identity)
          .collect::<Vec<_>>()
      });
    assert_eq!(
      named,
      Some(vec![fresh]),
      "only the new window is named, by its identity"
    );
  }

  /// Every identity `store` holds, with its holders and its form.
  fn held(store: &ContentStore) -> Vec<(ContentId, u64, Option<Form>)> {
    store
      .identities()
      .map(|identity| (*identity, store.holders(identity), store.form(identity)))
      .collect()
  }

  /// A-727 slice c (an object put a window at a time: §4.17.1 "A push's intake", rule 1). Do: for objects of 0, 1, a
  /// window less one, a window, a window and one, and seven windows and five bytes, put each into one store whole
  /// (`put_object`) and into another a piece at a time (pieces of 1, 3 and 61 bytes, and whole), each twice. Expect the
  /// same root, the same contents with the same holders and forms, the same bytes used, the object's bytes read back;
  /// and an object abandoned part-way leaves its store as it was.
  #[test]
  fn an_object_put_a_window_at_a_time_is_the_object_put_whole() {
    let bytes: Vec<u8> = (0..7 * NARROW + 5).map(|at| (at * 7 % 251) as u8).collect();
    for len in [0, 1, NARROW - 1, NARROW, NARROW + 1, bytes.len()] {
      let object = &bytes[..len];
      for piece in [1, 3, 61, len.max(1)] {
        let mut whole = ContentStore::new(1 << 20, NARROW).unwrap();
        let mut streamed = ContentStore::new(1 << 20, NARROW).unwrap();
        let mut roots = Vec::new();
        for _ in 0..2 {
          let root = whole.put_object(object).unwrap();
          let mut writing = streamed.begin_object();
          for part in object.chunks(piece) {
            streamed.put_window(&mut writing, part).unwrap();
          }
          let made = streamed.finish_object(&mut writing).unwrap();
          roots.push((root, made));
        }
        assert!(
          roots.iter().all(|(root, made)| root == made),
          "{len} by {piece}"
        );
        assert_eq!(held(&whole), held(&streamed), "{len} by {piece}");
        assert_eq!(whole.used(), streamed.used(), "{len} by {piece}");
        let (root, _) = roots[0];
        assert_eq!(
          streamed.object_bytes(&root, len as u64).as_deref(),
          Some(object),
          "{len} by {piece}"
        );
      }
    }
    let mut store = ContentStore::new(1 << 20, NARROW).unwrap();
    let mut writing = store.begin_object();
    store
      .put_window(&mut writing, &bytes[..3 * NARROW + 2])
      .unwrap();
    assert_eq!(writing.windows_kept(), 3);
    store.abandon_object(writing);
    assert_eq!((store.len(), store.used()), (0, 0));
  }

  /// The identity rule (main's ruling, 2026-11-20; R8): an object's identity is a pure function of its bytes, plain
  /// BLAKE3, whatever window the store cuts it at (a node's window is `16 x` its base page). Do: put the same object of
  /// 40 windows of the narrower store, its bytes streamed in odd pieces too, into a store of a 64-byte window and one of
  /// a 256-byte window; read it back by the identity each answered; release it. Expect: both answers, `object_id`'s and
  /// the streamed put's, the BLAKE3 of the bytes; each store reading the object whole by it and holding it twice (put
  /// and streamed); released twice, gone from both.
  #[test]
  fn an_objects_identity_is_the_blake3_of_its_bytes_whatever_the_stores_window() {
    let bytes: Vec<u8> = (0..40 * 64)
      .map(|at| u8::try_from(at * 7 % 251).unwrap())
      .collect();
    let pure = ContentId::of(&bytes);
    for window in [64, 256] {
      let mut store = ContentStore::new(1 << 20, window).unwrap();
      assert_eq!(store.object_id(&bytes), pure, "window {window}");
      assert_eq!(store.put_object(&bytes).unwrap(), pure, "window {window}");
      let mut writing = store.begin_object();
      for piece in bytes.chunks(37) {
        store.put_window(&mut writing, piece).unwrap();
      }
      assert_eq!(
        store.finish_object(&mut writing).unwrap(),
        pure,
        "window {window}"
      );
      let len = u64::try_from(bytes.len()).unwrap();
      assert_eq!(store.object_bytes(&pure, len), Some(bytes.clone()));
      assert_eq!(store.holders(&pure), 2);
      store.release(&pure).unwrap();
      store.release(&pure).unwrap();
      assert_eq!(store.object_bytes(&pure, len), None);
      assert_eq!(store.len(), 0, "window {window}: nothing left");
    }
  }
}
