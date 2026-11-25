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

use crate::preimage::{PreimageCounts, Preimages};

/// Format: a content identity's bytes, BLAKE3's output length.
pub const IDENTITY_BYTES: usize = blake3::OUT_LEN;

/// What a store kept or let go since its owner last took it ([`ContentStore::changes`]): each list with its bytes, none
/// once freed; each name with its root, none once unnamed.
pub type Changes<'a> = (
  Vec<(ContentId, Option<&'a [u8]>)>,
  Vec<(ContentId, Option<ContentId>)>,
);

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
  /// `need` more bytes asked for in a step whose room its owner could not grow, since it asked inside a lend of the
  /// chunks the windows live in, where the pool they grow from is out of reach ([`ContentStore::set_budget_out_of_reach`]):
  /// refused whatever room is left, so the misuse fails at its first put, never only once the slack runs out.
  OutOfReach {
    /// The bytes the request needed.
    need: u64,
  },
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
  /// [`WindowStorage::read`], given what this storage keeps its windows in when none is lent to it now (A-760: a
  /// recovery's replay reads a work's named text read-only, the shard's chunk store in its place). A storage that keeps
  /// its windows itself reads them as [`WindowStorage::read`] does.
  fn read_in<'a>(
    &'a self,
    window: StoredWindow,
    _kept_in: &'a dyn std::any::Any,
  ) -> Option<Cow<'a, [u8]>> {
    self.read(window)
  }
  /// Copies `into.len()` bytes kept under `window` from `offset` into `into`, nothing else of the window copied (A-760:
  /// a read a slice long costs the slice); `kept_in` as [`WindowStorage::read_in`]. None past the window's bytes.
  fn read_range_in(
    &self,
    window: StoredWindow,
    (offset, into): (usize, &mut [u8]),
    kept_in: &dyn std::any::Any,
  ) -> Option<()> {
    let bytes = self.read_in(window, kept_in)?;
    let from = bytes.get(offset..offset.checked_add(into.len())?)?;
    into.copy_from_slice(from);
    Some(())
  }
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

/// The lists read at a cut, and where the read stopped short of its range ([`ContentStore::lists_at_cut`]).
pub type CutLists = (Vec<(ContentId, Box<[u8]>)>, Option<u64>);

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
  /// Let go (A-724 piece 4, [`ContentStore::let_go`]): a window whose bytes a landed changeset on the node's store
  /// holds, kept here no longer; a read of it finds nothing until [`ContentStore::bring_back`] or a put brings it back.
  Away {
    /// The window's length.
    len: usize,
  },
}

/// What a content store holds ([`ContentStore::census`]): each kind of entry's count and bytes, by where its bytes are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentCensus {
  /// Windows kept by the window storage (the shard's chunk store, or the heap where it has none).
  pub stored: u64,
  /// Their bytes.
  pub stored_bytes: u64,
  /// Windows let go (A-724 piece 4): kept on the node's store, not here.
  pub away: u64,
  /// Their bytes.
  pub away_bytes: u64,
  /// Windows kept in the store's own memory.
  pub here_windows: u64,
  /// Their bytes.
  pub here_window_bytes: u64,
  /// Lists, in the store's own memory.
  pub lists: u64,
  /// Their bytes.
  pub list_bytes: u64,
  /// Index tables, in the store's own memory.
  pub tables: u64,
  /// Their bytes.
  pub table_bytes: u64,
  /// Derived nodes, in the store's own memory.
  pub derived: u64,
  /// Their bytes.
  pub derived_bytes: u64,
  /// Objects named by their identity.
  pub objects: u64,
  /// The bytes counted against the store's budget.
  pub used: u64,
  /// The bytes structures beside it charged to it.
  pub charged: u64,
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
  /// Windows let go to the node's store (A-724 piece 4, [`ContentStore::let_go`]).
  pub let_go: u64,
  /// Windows let go and brought back ([`ContentStore::bring_back`], or a put of the same bytes).
  pub brought_back: u64,
  /// Windows a let-go kept because an unlanded holder names them ([`ContentStore::pin_unlanded`]): the rule's
  /// non-vacuity counter.
  pub kept_unlanded: u64,
  /// Windows found let go as their unlanded holder pinned them ([`ContentStore::pin_unlanded`]): its bytes gone before
  /// it landed, the very fault the pin exists against, so a named bug signal and never a later unknown base.
  pub pinned_away: u64,
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
  /// Whether a landed changeset on the node's store holds its bytes ([`ContentStore::mark_landed`]): only such a
  /// window may be let go.
  landed: bool,
  /// How many objects that no landed changeset holds yet name it ([`ContentStore::pin_unlanded`]): while any does, no
  /// let-go takes it, whatever its holders.
  unlanded: u32,
  /// The store's read count at its last read ([`ContentStore::last_read`]): the coldest is let go first.
  last_read: core::cell::Cell<u64>,
}

impl Held {
  /// The bytes kept in the store's own memory: a list's, a table's or a derived node's; none for a window.
  fn here(&self) -> Option<&[u8]> {
    match &self.kept {
      Kept::Here(bytes) => Some(bytes),
      Kept::Stored { .. } | Kept::Away { .. } => None,
    }
  }

  /// Its length in bytes.
  fn len(&self) -> usize {
    match &self.kept {
      Kept::Here(bytes) => bytes.len(),
      Kept::Stored { len, .. } | Kept::Away { len } => *len,
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

  /// The buffer the object's windows were filled in, taken out, emptied: what a writer that puts object after object
  /// hands the next one ([`ContentStore::begin_object_reusing`]). Taken from a writer not yet finished, its pending
  /// bytes go with it, so it is taken once the object is finished or before it is abandoned.
  pub fn take_buffer(&mut self) -> Vec<u8> {
    let mut buffer = std::mem::take(&mut self.pending);
    buffer.clear();
    buffer
  }
}

/// A paged family of the content store's, as a shard image's cut freezes it (A-870 piece 2, F2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutFamily {
  /// The lists, by identity.
  Lists,
  /// The names, by the object's identity.
  Names,
  /// The windows let go, by identity.
  Aways,
}

/// The content store's pre-images of an image cut, a family each ([`crate::preimage`]).
#[derive(Debug, Default)]
struct CutPreimages {
  /// The lists' bytes at the cut.
  lists: Preimages<ContentId, Box<[u8]>>,
  /// The names' roots at the cut.
  names: Preimages<ContentId, ContentId>,
  /// The windows let go at the cut, by length.
  aways: Preimages<ContentId, u64>,
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
  /// The lists kept or freed since its owner last took the changes (A-727 slice a3b: the shard image carries them).
  lists_changed: BTreeSet<ContentId>,
  /// The objects named or unnamed since its owner last took the changes (A-727 slice a3b).
  names_changed: BTreeSet<ContentId>,
  /// The windows let go, brought back or freed away since its owner last took the changes (A-767 C1 step 2b): the
  /// shard image carries every window let go, so a restart keeps it as let go, never as absent.
  away_changed: BTreeSet<ContentId>,
  /// The image cut's pre-images of the lists, names and windows let go (A-870 piece 2, F2; [`crate::preimage`]): each
  /// entry's value at the cut, kept before it changes in a page the cut has not yet encoded.
  cut: CutPreimages,
  /// Where the windows' bytes are kept (A-727).
  storage: Box<dyn WindowStorage>,
  /// The longest content the store holds: the content plane's window.
  window: NonZeroUsize,
  /// The identities one list names at most: the window over an identity's bytes, at least two.
  fanout: NonZeroUsize,
  budget: u64,
  /// Whether the budget is one its owner could not grow for the step ([`ContentStore::set_budget_out_of_reach`]): every
  /// put that needs a byte is refused [`ContentRefused::OutOfReach`] until the next [`ContentStore::set_budget`].
  out_of_reach: bool,
  /// The bytes of every content held.
  content_bytes: u64,
  /// The bytes structures beside the store charged to it.
  charged_bytes: u64,
  counters: ContentCounters,
  /// The miss-ratio curve of the repository a step is lent to, while it is (A-724 piece 2d; `crate::mrc`): each read
  /// noted in it ([`ContentStore::observe`]). A cell, since reads borrow the store shared; one shard's thread holds it.
  observing: core::cell::RefCell<Option<crate::mrc::Curve>>,
  /// Reads noted so far: the clock a content's last read is stamped by (A-724 piece 4).
  reads: core::cell::Cell<u64>,
}

/// Why a window was not let go ([`ContentStore::let_go`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LetGoRefused {
  /// The store holds no content of that identity.
  NotHeld,
  /// It is no window (a list, a table or a derived node: the store's own metadata, kept in memory).
  NotAWindow,
  /// No landed changeset holds its bytes yet ([`ContentStore::mark_landed`]): letting it go would lose it.
  NotLanded,
  /// More than one holder keeps it: content another repository holds is never let go for one past its grant.
  Shared,
  /// Already let go.
  Away,
  /// A holder of it cannot bring it back by its own read path ([`ContentStore::let_go_landed`]): it stays.
  Pinned {
    /// The holder that pins it.
    by: &'static str,
  },
  /// It holds no bytes (the empty window: an empty file's, or a Mercurial manifest kept as an empty delta): letting it go
  /// would free nothing, and a shard image carries no window let go of no length ([`ContentStore::restore_away`]), so
  /// one let go would be lost at a restart while a row still names it. It stays held.
  Empty,
  /// An object no landed changeset holds yet names it ([`ContentStore::pin_unlanded`]): letting it go would lose bytes
  /// that object still needs and no node's store keeps for it. It stays held.
  Unlanded,
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
    Kept::Here(_) | Kept::Away { .. } => None,
  }
}

/// What giving back a content's last count frees: its bytes and its form, or nothing while other holders remain.
/// What one count given back freed, if it was the last: the content's form, and its bytes where someone reads them (a
/// list's and a derived node's, what the cascade frees next; a table's, whose rows its holder gives back); a window's
/// bytes are no one's once it is freed, so none.
type Released = Option<(Option<Box<[u8]>>, Form)>;

/// What [`ContentStore::release`] did (goal 93, P7 finding (2): it read every freed window back out of its storage and
/// copied it for an answer no caller used).
#[derive(Debug, PartialEq, Eq)]
pub enum Freed {
  /// A holder is left: nothing was freed.
  Held,
  /// The content is freed with everything it named; its bytes are no one's.
  Gone,
  /// A table is freed: its bytes, whose rows its holder gives back (`crate::index`).
  Table(Box<[u8]>),
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
      objects: BTreeMap::new(),
      rooted: BTreeMap::new(),
      lists_changed: BTreeSet::new(),
      names_changed: BTreeSet::new(),
      away_changed: BTreeSet::new(),
      cut: CutPreimages::default(),
      storage: Box::new(HeapWindows::default()),
      window,
      fanout,
      budget,
      out_of_reach: false,
      content_bytes: 0,
      charged_bytes: 0,
      counters: ContentCounters::default(),
      observing: core::cell::RefCell::new(None),
      reads: core::cell::Cell::new(0),
    })
  }

  /// The window: the longest content the store holds.
  pub fn window(&self) -> usize {
    self.window.get()
  }

  /// Sets the budget the caller derived again. Nothing held is dropped; past it, new bytes are refused.
  pub fn set_budget(&mut self, budget: u64) {
    self.budget = budget;
    self.out_of_reach = false;
  }

  /// Sets `budget` as one its owner could not grow for the step: room was asked inside a lend of the chunks the windows
  /// live in, where the pool they grow from is out of reach (the server's `vcs_store::lent_state`). Every put that needs
  /// a byte is refused [`ContentRefused::OutOfReach`], whatever room is left, until the next
  /// [`ContentStore::set_budget`]; reads and frees go on.
  pub fn set_budget_out_of_reach(&mut self, budget: u64) {
    self.budget = budget;
    self.out_of_reach = true;
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

  /// Refuses `need` more bytes unless the budget has room for them once `freed` charged bytes are given back, and any
  /// byte at all while the budget is out of reach ([`ContentStore::set_budget_out_of_reach`]).
  fn admit(&self, need: u64, freed: u64) -> Result<(), ContentRefused> {
    if self.out_of_reach && need > 0 {
      return Err(ContentRefused::OutOfReach { need });
    }
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
      // A window let go is brought back by its own bytes put again (A-724 piece 4).
      if self.is_away(&identity) {
        self.bring_back(&identity, bytes)?;
      }
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
    if form == Form::List {
      self.before_list_change(identity);
    }
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
        landed: false,
        unlanded: 0,
        last_read: core::cell::Cell::new(0),
      },
    );
    self.content_bytes = content_bytes;
    self.counters.stored = self.counters.stored.saturating_add(1);
    if form == Form::List {
      self.counters.lists = self.counters.lists.saturating_add(1);
      self.lists_changed.insert(identity);
    }
    Ok(())
  }

  /// The identity an object of `bytes` is named by: the plain BLAKE3 of its bytes, whatever their length or the store's
  /// window (module docs, "The identity rule"); the root it is held under is the store's own ([`ContentStore::root_of`]).
  /// Nothing is stored.
  pub fn object_id(&self, bytes: &[u8]) -> ContentId {
    ContentId::of(bytes)
  }

  /// The node an object's identity names in this store, its root (module docs, "The identity rule"). Node-local: never
  /// sent or recorded.
  pub fn root_of(&self, identity: &ContentId) -> ContentId {
    self.resolve(identity)
  }

  /// `identity`, the BLAKE3 of an object's bytes, named for `root`, the tree this store keeps it as (module docs, "The
  /// identity rule"): a root that is the identity itself (an object of at most one window) needs no name. The identity.
  fn named_object(&mut self, identity: ContentId, root: ContentId) -> ContentId {
    if identity != root && !self.objects.contains_key(&identity) {
      self.before_name_change(identity);
      self.objects.insert(identity, root);
      self.rooted.insert(root, identity);
      self.names_changed.insert(identity);
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
  ///
  /// A window the store holds let go stays let go: a put under a root or list held already holds it once more and
  /// keeps nothing below it, so a record's replay after a warm restart takes its holds and leaves the windows the image
  /// let go out of RAM (A-767 C1 step 2b). A put that must read whole is [`ContentStore::put_object_whole`].
  pub fn put_object(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
    let root = self.put_object_root(bytes, false)?;
    Ok(self.named_object(ContentId::of(bytes), root))
  }

  /// [`ContentStore::put_object`], and each of its windows the store holds let go brought back from `bytes`, which are
  /// its own (codex-review, 2026-11-22): what a recovery that read a payload from the node's store puts, so the payload
  /// reads whole by its identity, as a put that answers its identity promises. The windows are brought back first, so a
  /// refusal takes no hold.
  pub fn put_object_whole(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
    let root = self.put_object_root(bytes, true)?;
    Ok(self.named_object(ContentId::of(bytes), root))
  }

  /// [`ContentStore::put_object`]'s tree: its root; `whole`, each of its windows held let go brought back from `bytes`
  /// ([`ContentStore::put_object_whole`]).
  fn put_object_root(&mut self, bytes: &[u8], whole: bool) -> Result<ContentId, ContentRefused> {
    if bytes.len() <= self.window.get() {
      return self.put(bytes);
    }
    let tree = Tree::plan(bytes, self.window, self.fanout);
    let top = tree.levels.len().saturating_sub(1);
    let root = tree.root().ok_or(ContentRefused::NotHeld)?.identity;
    // A window the store holds let go comes back from these bytes, which are its own, where the put must read whole: a
    // put under a root or list held already holds it once more and keeps nothing below it, so a payload read from disk
    // after a recovery put back only its new windows and read none of the rest (codex-review, 2026-11-22). First, so a
    // refusal here leaves no hold taken.
    if whole {
      self.bring_back_planned(&tree)?;
    }
    let need = self.new_bytes(&tree, top)?;
    self.admit(need, 0)?;
    self.keep_tree(&tree, top)?;
    Ok(root)
  }

  /// Brings back each of `tree`'s windows the store holds let go ([`ContentStore::bring_back`]), from the planned bytes,
  /// whose identities the plan has named already: no window is hashed again.
  fn bring_back_planned(&mut self, tree: &Tree<'_>) -> Result<(), ContentRefused> {
    for node in tree.levels.first().into_iter().flatten() {
      if self.is_away(&node.identity) {
        self.bring_back(&node.identity, node.bytes())?;
      }
    }
    Ok(())
  }

  /// Starts an object put a window at a time ([`ContentStore::put_window`], [`ContentStore::finish_object`]): what an
  /// intake streams, its bytes arriving in pieces it never holds whole (§4.17.1 "A push's intake", rule 1; Subversion's
  /// commit texts, A-727 slice c). The same object [`ContentStore::put_object`] keeps, the same root, its windows kept as
  /// each fills. For an object whose length is not known until its last piece; one whose length is known begins with
  /// [`ContentStore::begin_object_of_length`].
  pub fn begin_object(&self) -> Writing {
    Writing::default()
  }

  /// Starts an object of `len` bytes put a window at a time, as [`ContentStore::begin_object`] does, its window's buffer
  /// reserved once now at the window or `len` if shorter, so no piece grows or moves it (goal 93, svn finding (2)):
  /// begun empty, the buffer doubled from the first piece, and a 256 KiB text put in 65 KB copy slices made allocations
  /// of 65, 130, 260 and 520 KB, 3.7 times the text, each below the large-path floor the system allocator keeps
  /// (A-742). An object longer than `len` stays correct, its buffer growing past the reservation as an unsized one would.
  pub fn begin_object_of_length(&self, len: u64) -> Writing {
    let window = self.window.get();
    Writing {
      pending: Vec::with_capacity(usize::try_from(len).map_or(window, |len| len.min(window))),
      ..Writing::default()
    }
  }

  /// Starts an object of `len` bytes (none: not known) as [`ContentStore::begin_object_of_length`] does, its window's
  /// buffer `buffer`, a previous object's ([`Writing::take_buffer`]): a writer that puts object after object (an
  /// import's texts) owns one buffer and hands it from each to the next, so no object allocates one of its own (P7
  /// finding (2), main 01a20182's routing of 2026-11-22 07:38: one buffer a text was among the import's churners).
  /// Where it is shorter than the object needs (the window, or `len` if shorter), it is grown once, to the window, so
  /// it grows at most once over the writer's life.
  pub fn begin_object_reusing(&self, len: Option<u64>, mut buffer: Vec<u8>) -> Writing {
    let window = self.window.get();
    let wanted = len.map_or(window, |len| {
      usize::try_from(len).map_or(window, |len| len.min(window))
    });
    buffer.clear();
    if buffer.capacity() < wanted {
      buffer.reserve_exact(window);
    }
    Writing {
      pending: buffer,
      ..Writing::default()
    }
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
      // A window let go is brought back by its own bytes put again (A-724 piece 4).
      if self.is_away(&identity) {
        self.bring_back(&identity, bytes)?;
      }
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

  /// Gives back one holder's count of `identity`: whether that was its last holder, which frees it. A freed list gives
  /// back what it named, and a freed derived node what it names, and each list or node freed by that what it names,
  /// until nothing more is freed; past a refusal there the rest is given back still, and the first refusal is the
  /// answer. The cascade is one call, as long as what it frees (slicing it is owed with A-581's). A freed window is
  /// never read back (goal 93, P7 finding (2): every one was copied out of its storage for an answer no caller used).
  pub fn release(&mut self, identity: &ContentId) -> Result<Freed, ContentRefused> {
    let identity = &self.resolve(identity);
    let Some((bytes, form)) = self.release_one(identity)? else {
      return Ok(Freed::Held);
    };
    let mut named: Vec<ContentId> = Vec::new();
    given_back(form, bytes.as_deref().unwrap_or_default(), &mut named);
    let mut first = Ok(());
    while let Some(next) = named.pop() {
      // A derived node names an object by its identity; a list names its own windows and lists.
      let next = self.resolve(&next);
      match self.release_one(&next) {
        Ok(Some((freed, form))) => {
          given_back(form, freed.as_deref().unwrap_or_default(), &mut named)
        }
        Ok(None) => {}
        Err(refused) => first = first.and(Err(refused)),
      }
    }
    first.map(|()| match (form, bytes) {
      (Form::Table, Some(bytes)) => Freed::Table(bytes),
      _ => Freed::Gone,
    })
  }

  /// One count of `identity` given back; its form, and its bytes where the cascade reads them ([`Released`]), when it was
  /// the last.
  fn release_one(&mut self, identity: &ContentId) -> Result<Released, ContentRefused> {
    let held = self
      .entries
      .get_mut(identity)
      .ok_or(ContentRefused::NotHeld)?;
    if held.holders > 1 {
      held.holders = held.holders.saturating_sub(1);
      return Ok(None);
    }
    // Only the families the entry is in change with it, so no other family keeps a marker for it.
    let (list, away) = (
      held.form == Form::List,
      matches!(held.kept, Kept::Away { .. }),
    );
    if list {
      self.before_list_change(*identity);
    }
    if away {
      self.before_away_change(*identity);
    }
    if let Some(named) = self.rooted.get(identity).copied() {
      self.before_name_change(named);
    }
    let Some(freed) = self.entries.remove(identity) else {
      return Err(ContentRefused::NotHeld);
    };
    if matches!(freed.kept, Kept::Away { .. }) {
      self.away_changed.insert(*identity);
    }
    // A freed root takes its object's name with it (module docs, "The identity rule").
    if let Some(named) = self.rooted.remove(identity) {
      self.objects.remove(&named);
      self.names_changed.insert(named);
    }
    if freed.form == Form::List {
      self.lists_changed.insert(*identity);
    }
    let len = width(freed.len())?;
    self.content_bytes = self.content_bytes.saturating_sub(len);
    self.counters.freed = self.counters.freed.saturating_add(1);
    // Only a list's, a derived node's and a table's bytes are read: they name what is given back next.
    let named = matches!(freed.form, Form::List | Form::Derived | Form::Table);
    let bytes = self.freed_bytes(freed.kept, named);
    Ok(Some((bytes, freed.form)))
  }

  /// A freed entry's bytes where the cascade reads them (`named`), its storage window let go either way.
  fn freed_bytes(&mut self, kept: Kept, named: bool) -> Option<Box<[u8]>> {
    match kept {
      Kept::Here(bytes) => named.then_some(bytes),
      // Its bytes are the node's store's: nothing kept here to free or to give back.
      Kept::Away { .. } => None,
      Kept::Stored { window, .. } => {
        let bytes = if named {
          self.storage.read(window).map(|read| Box::from(&*read))
        } else {
          None
        };
        self.storage.release(window);
        bytes
      }
    }
  }

  /// The bytes of the content `identity` names: a window's, a list's identities, or a table's. Lists and tables are the
  /// store's own metadata and stay in its memory, so they are lent; an object's windows are read through
  /// [`ContentStore::windows`] and [`ContentStore::window_at`], which answer a `Cow` (A-727: a window kept as a sealed
  /// chunk of the shard's arena is copied out to be read).
  pub fn get(&self, identity: &ContentId) -> Option<&[u8]> {
    let identity = &self.resolve(identity);
    let held = self.entries.get(identity)?;
    self.note(identity, held);
    held.here()
  }

  /// Lends `curve` to the store, every read noted in it until the next call, and gives back the curve it held: what a
  /// repository's step reads is that repository's working set (A-724 piece 2d, §4.17.5 "Learned demand"). The caller
  /// brackets a step with it, putting back what it took, so a step inside a step restores its outer curve.
  pub fn observe(&self, curve: Option<crate::mrc::Curve>) -> Option<crate::mrc::Curve> {
    match self.observing.try_borrow_mut() {
      Ok(mut slot) => core::mem::replace(&mut *slot, curve),
      // Borrowed only inside a read's note, which never calls this: the curve goes back to its caller unlent.
      Err(_) => curve,
    }
  }

  /// Marks the window `identity` names as held by a landed changeset on the node's store (A-724 piece 4): its owner
  /// calls this once the changeset carrying it is durable there, after which the window may be let go
  /// ([`ContentStore::let_go`]). Whether the store holds such a window.
  pub fn mark_landed(&mut self, identity: &ContentId) -> bool {
    let identity = self.resolve(identity);
    match self.entries.get_mut(&identity) {
      Some(held) if held.form == Form::Window => {
        held.landed = true;
        true
      }
      _ => false,
    }
  }

  /// Pins every window of the object `identity` names (its layout, let go or not) for an unlanded holder of it: an object
  /// a write kept that no landed changeset on the node's store holds yet (goal 93, main's ruling (B) of 2026-11-24: a
  /// landed segment's let-go took a window a later, unlanded revision of the same push shared by content, and that one's
  /// next reader found it away and asked a store that keeps no row of it). While pinned, no let-go takes the window,
  /// whatever its holders ([`LetGoRefused::Unlanded`]); its holder unpins it once it lands, or however its write ends
  /// ([`ContentStore::unpin_unlanded`]). The windows pinned.
  ///
  /// A window found let go as it is pinned is pinned all the same (a put brings it back with its count) and counted
  /// `pinned_away`: its unlanded holder's bytes were already gone, the fault the pin exists against.
  ///
  /// # Errors
  /// `NotHeld` where the store holds no such object or a window of it: nothing pinned. `CountExhausted` where a
  /// window's count would pass its width: nothing pinned.
  pub fn pin_unlanded(&mut self, identity: &ContentId) -> Result<usize, ContentRefused> {
    if !self.contains(identity) {
      return Err(ContentRefused::NotHeld);
    }
    let windows: Vec<ContentId> = self
      .object_layout(identity)
      .into_iter()
      .map(|(window, _)| self.resolve(&window))
      .collect();
    let mut refused = None;
    for window in &windows {
      match self.entries.get(window) {
        None => refused = Some(ContentRefused::NotHeld),
        Some(held) if held.unlanded == u32::MAX => {
          refused = refused.or(Some(ContentRefused::CountExhausted));
        }
        Some(_) => {}
      }
    }
    if let Some(refused) = refused {
      return Err(refused);
    }
    let mut pinned = 0usize;
    let mut away = 0u64;
    for window in &windows {
      if let Some(held) = self.entries.get_mut(window)
        && held.form == Form::Window
      {
        held.unlanded = held.unlanded.saturating_add(1);
        pinned = pinned.saturating_add(1);
        if matches!(held.kept, Kept::Away { .. }) {
          away = away.saturating_add(1);
        }
      }
    }
    self.counters.pinned_away = self.counters.pinned_away.saturating_add(away);
    Ok(pinned)
  }

  /// Unpins every window of the object `identity` names, once each, as [`ContentStore::pin_unlanded`] pinned them: its
  /// unlanded holder landed, or its write ended. A window no longer pinned may be let go again.
  ///
  /// Precondition: unpin before the holder releases the object. A pinned window freed (its last holder released) takes
  /// its count with it and a window put again starts at none, so an unpin after the release would find no count, or
  /// take a later pinner's.
  pub fn unpin_unlanded(&mut self, identity: &ContentId) {
    let windows: Vec<ContentId> = self
      .object_layout(identity)
      .into_iter()
      .map(|(window, _)| self.resolve(&window))
      .collect();
    for window in &windows {
      if let Some(held) = self.entries.get_mut(window)
        && held.form == Form::Window
      {
        held.unlanded = held.unlanded.saturating_sub(1);
      }
    }
  }

  /// Lets go the bytes of the window `identity` names, past its repository's memory grant (A-724 piece 4, §4.17.5
  /// "The cold trigger"): only a window a landed changeset holds ([`ContentStore::mark_landed`]) and one holder alone
  /// keeps, so content another repository holds is never let go here. Its entry stays (its holders, form and length),
  /// its bytes leave the window storage, and a read of it finds nothing until it is brought back
  /// ([`ContentStore::bring_back`], or a put of the same bytes). The bytes freed.
  ///
  /// # Errors
  /// [`LetGoRefused`] by kind, the store unchanged.
  pub fn let_go(&mut self, identity: &ContentId) -> Result<u64, LetGoRefused> {
    let identity = self.resolve(identity);
    let held = self
      .entries
      .get_mut(&identity)
      .ok_or(LetGoRefused::NotHeld)?;
    let (window, len) = match held.kept {
      Kept::Stored { window, len } if held.form == Form::Window => (window, len),
      Kept::Away { .. } => return Err(LetGoRefused::Away),
      _ => return Err(LetGoRefused::NotAWindow),
    };
    if !held.landed {
      return Err(LetGoRefused::NotLanded);
    }
    if len == 0 {
      return Err(LetGoRefused::Empty);
    }
    if held.unlanded > 0 {
      self.counters.kept_unlanded = self.counters.kept_unlanded.saturating_add(1);
      return Err(LetGoRefused::Unlanded);
    }
    if held.holders > 1 {
      return Err(LetGoRefused::Shared);
    }
    Ok(self.released_away(identity, window, len))
  }

  /// Lets go of the window `identity` names, marked landed ([`ContentStore::mark_landed`]: a landed changeset on the
  /// node's store holds its bytes), whatever its holders count, unless `pinned_by` names a holder of it that cannot bring
  /// it back by its own read path (main 01a20182's ruling of 2026-11-22 06:58: a landed window may go only where every
  /// holder of it can). What [`ContentStore::let_go`] refuses `Shared` this lets go: the holders are a landed push's own
  /// (its staged pack, its repository's pack), each a reader that brings the window back by its pack's identity. The
  /// store keeps no holder's kind, so its owner names the pinning one (a home, read by the window's identity alone, by
  /// where the identity's home is: `slates-server`'s homes), which holds across a restart. The bytes freed.
  ///
  /// # Errors
  /// `NotHeld`, `NotAWindow`, `Away`, `NotLanded` and `Empty` as [`ContentStore::let_go`]; `Pinned` naming the holder.
  pub fn let_go_landed(
    &mut self,
    identity: &ContentId,
    pinned_by: Option<&'static str>,
  ) -> Result<u64, LetGoRefused> {
    let identity = self.resolve(identity);
    let held = self
      .entries
      .get_mut(&identity)
      .ok_or(LetGoRefused::NotHeld)?;
    let (window, len) = match held.kept {
      Kept::Stored { window, len } if held.form == Form::Window => (window, len),
      Kept::Away { .. } => return Err(LetGoRefused::Away),
      _ => return Err(LetGoRefused::NotAWindow),
    };
    if !held.landed {
      return Err(LetGoRefused::NotLanded);
    }
    if len == 0 {
      return Err(LetGoRefused::Empty);
    }
    // Whatever its holders, never a window an unlanded object names (goal 93, main's ruling (B) of 2026-11-24).
    if held.unlanded > 0 {
      self.counters.kept_unlanded = self.counters.kept_unlanded.saturating_add(1);
      return Err(LetGoRefused::Unlanded);
    }
    if let Some(by) = pinned_by {
      return Err(LetGoRefused::Pinned { by });
    }
    Ok(self.released_away(identity, window, len))
  }

  /// Every window of the object `identity` names marked landed and let go ([`ContentStore::let_go_landed`]), each but
  /// those `pinned_by` names a pinning holder of and those an unlanded object names ([`ContentStore::pin_unlanded`]): the
  /// bytes freed and the windows kept.
  pub fn let_go_object_landed(
    &mut self,
    identity: &ContentId,
    pinned_by: &dyn Fn(&ContentId) -> Option<&'static str>,
  ) -> (u64, u64) {
    let (mut freed, mut pinned) = (0u64, 0u64);
    for window in self.object_windows(identity) {
      if !self.mark_landed(&window) {
        continue;
      }
      match self.let_go_landed(&window, pinned_by(&window)) {
        Ok(bytes) => freed = freed.saturating_add(bytes),
        Err(LetGoRefused::Pinned { .. } | LetGoRefused::Unlanded) => {
          pinned = pinned.saturating_add(1)
        }
        Err(_) => {}
      }
    }
    (freed, pinned)
  }

  /// The windows of the object `identity` names, in order: itself where it is one window.
  fn object_windows(&self, identity: &ContentId) -> Vec<ContentId> {
    let mut windows = Vec::new();
    let mut walk = self.windows(identity);
    while let Some(window) = walk.next_window() {
      windows.push(window);
    }
    windows
  }

  /// Marks the window `identity` (kept as `window`, `len` bytes) let go, its storage given back: the bytes freed.
  fn released_away(&mut self, identity: ContentId, window: StoredWindow, len: usize) -> u64 {
    self.before_away_change(identity);
    if let Some(held) = self.entries.get_mut(&identity) {
      held.kept = Kept::Away { len };
    }
    self.away_changed.insert(identity);
    self.storage.release(window);
    let freed = u64::try_from(len).unwrap_or(u64::MAX);
    self.content_bytes = self.content_bytes.saturating_sub(freed);
    self.counters.let_go = self.counters.let_go.saturating_add(1);
    freed
  }

  /// Brings back a window let go, from `bytes` its reader read from the node's store (A-724 piece 4; the tools'
  /// read-through, w-git's and w-hg's): they must be the window's own, by its identity and length, and fit the budget.
  ///
  /// # Errors
  /// `NotHeld` for a window not let go here or for bytes that are not its own (by identity or length); the budget's
  /// refusal past it, as a put's.
  pub fn bring_back(&mut self, identity: &ContentId, bytes: &[u8]) -> Result<(), ContentRefused> {
    let identity = self.resolve(identity);
    let len = match self.entries.get(&identity).map(|held| &held.kept) {
      Some(Kept::Away { len }) => *len,
      _ => return Err(ContentRefused::NotHeld),
    };
    if bytes.len() != len || ContentId::named(Form::Window, bytes) != identity {
      return Err(ContentRefused::NotHeld);
    }
    let need = width(len)?;
    self.admit(need, 0)?;
    let window = self.storage.put(bytes)?;
    self.storage.named(window, &identity);
    self.before_away_change(identity);
    if let Some(held) = self.entries.get_mut(&identity) {
      held.kept = Kept::Stored { window, len };
    }
    self.away_changed.insert(identity);
    self.content_bytes = self.content_bytes.saturating_add(need);
    self.counters.brought_back = self.counters.brought_back.saturating_add(1);
    Ok(())
  }

  /// Marks every window of the object `identity` names landed and lets go each one it alone holds (A-767 C1 step 2b: a
  /// Mercurial import's file revisions, once the changeset carrying them is durable on the node's store): the bytes
  /// freed. A window another holder keeps stays, as [`ContentStore::let_go`] keeps it (`Shared`), and one let go already
  /// stays away; the object's lists stay, so its windows' identities and lengths are still its own and a reader brings
  /// it back by its bytes ([`ContentStore::bring_back_object`]).
  pub fn let_go_object(&mut self, identity: &ContentId) -> u64 {
    let mut windows = Vec::new();
    let mut walk = self.windows(identity);
    while let Some(window) = walk.next_window() {
      windows.push(window);
    }
    let mut freed = 0u64;
    for window in &windows {
      if self.mark_landed(window)
        && let Ok(bytes) = self.let_go(window)
      {
        freed = freed.saturating_add(bytes);
      }
    }
    freed
  }

  /// Brings back every window let go of the object `identity` names ([`ContentStore::put_object`]'s identity), from
  /// `bytes`, the object's whole bytes its reader read from the node's store (A-767 C1: a Mercurial push's work reading a
  /// text an import let go): they must be the object's own, by its identity; each window is checked and admitted as
  /// [`ContentStore::bring_back`] checks and admits one, and a window held already is passed over. How many came back.
  ///
  /// # Errors
  /// `NotHeld` for bytes that are not the object's; a window's refusal as [`ContentStore::bring_back`]'s, the windows
  /// before it brought back.
  pub fn bring_back_object(
    &mut self,
    identity: &ContentId,
    bytes: &[u8],
  ) -> Result<u64, ContentRefused> {
    if ContentId::of(bytes) != *identity {
      return Err(ContentRefused::NotHeld);
    }
    let mut back = 0u64;
    for piece in bytes.chunks(self.window.get()) {
      let window = ContentId::of(piece);
      if self.is_away(&window) {
        self.bring_back(&window, piece)?;
        back = back.saturating_add(1);
      }
    }
    Ok(back)
  }

  /// Whether the window `identity` names was let go ([`ContentStore::let_go`]) and not brought back.
  pub fn is_away(&self, identity: &ContentId) -> bool {
    let identity = self.resolve(identity);
    matches!(
      self.entries.get(&identity).map(|held| &held.kept),
      Some(Kept::Away { .. })
    )
  }

  /// The store's read count at the last read of the content `identity` names (0 for none since it was kept): the
  /// coldest of a repository's windows is the one with the least ([`ContentStore::let_go`]'s caller orders by it).
  pub fn last_read(&self, identity: &ContentId) -> Option<u64> {
    let identity = self.resolve(identity);
    self.entries.get(&identity).map(|held| held.last_read.get())
  }

  /// One read of `held`, named `identity`, noted in the curve lent ([`ContentStore::observe`]): its key the identity's
  /// first eight bytes (uniform, BLAKE3's), its length the content's.
  fn note(&self, identity: &ContentId, held: &Held) {
    let tick = self.reads.get().saturating_add(1);
    self.reads.set(tick);
    held.last_read.set(tick);
    if let Ok(mut slot) = self.observing.try_borrow_mut()
      && let Some(curve) = slot.as_mut()
      && let Some(key) = identity.0.first_chunk::<{ size_of::<u64>() }>()
    {
      curve.read(
        u64::from_le_bytes(*key),
        u64::try_from(held.len()).unwrap_or(u64::MAX),
      );
    }
  }

  /// The bytes of the content `identity` names, whatever its form: a window's from the window storage (borrowed where
  /// it lends them, copied out where it cannot, A-727), a list's, table's or derived node's from the store's memory.
  pub fn bytes_of(&self, identity: &ContentId) -> Option<Cow<'_, [u8]>> {
    let identity = &self.resolve(identity);
    let held = self.entries.get(identity)?;
    self.note(identity, held);
    match &held.kept {
      Kept::Here(bytes) => Some(Cow::Borrowed(bytes)),
      Kept::Stored { window, .. } => self.storage.read(*window),
      Kept::Away { .. } => None,
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
          landed: false,
          unlanded: 0,
          last_read: core::cell::Cell::new(0),
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
    let freed_before = self.counters.freed;
    for identity in unheld {
      // Held by no one: one count stands in for the holder it never had, so release frees it, and a list gives back
      // the windows it names (a3b), each freed with its last.
      if let Some(held) = self.entries.get_mut(&identity) {
        if held.holders != 0 {
          continue;
        }
        held.holders = 1;
      }
      let _ = self.release(&identity);
    }
    self.counters.freed.saturating_sub(freed_before)
  }

  /// The identities whose first eight bytes, read big-endian, lie in `lo..=hi`: one page of a shard image's paged
  /// families (A-870), in identity order.
  fn identities_in(lo: u64, hi: u64) -> core::ops::RangeInclusive<ContentId> {
    let mut first = [0u8; IDENTITY_BYTES];
    let mut last = [u8::MAX; IDENTITY_BYTES];
    first[..8].copy_from_slice(&lo.to_be_bytes());
    last[..8].copy_from_slice(&hi.to_be_bytes());
    ContentId(first)..=ContentId(last)
  }

  /// [`ContentStore::lists`] whose identities' first eight bytes lie in `lo..=hi` (A-870: one page of the shard image).
  pub fn lists_in(&self, lo: u64, hi: u64) -> impl Iterator<Item = (ContentId, &[u8])> + '_ {
    self
      .entries
      .range(Self::identities_in(lo, hi))
      .filter(|(_, held)| held.form == Form::List)
      .filter_map(|(identity, held)| Some((*identity, held.here()?)))
  }

  /// [`ContentStore::names`] whose identities' first eight bytes lie in `lo..=hi` (A-870).
  pub fn names_in(&self, lo: u64, hi: u64) -> impl Iterator<Item = (ContentId, ContentId)> + '_ {
    self
      .objects
      .range(Self::identities_in(lo, hi))
      .map(|(identity, root)| (*identity, *root))
  }

  /// [`ContentStore::aways`] whose identities' first eight bytes lie in `lo..=hi` (A-870).
  pub fn aways_in(&self, lo: u64, hi: u64) -> impl Iterator<Item = (ContentId, u64)> + '_ {
    self
      .entries
      .range(Self::identities_in(lo, hi))
      .filter_map(|(identity, held)| match held.kept {
        Kept::Away { len } => Some((*identity, u64::try_from(len).unwrap_or(u64::MAX))),
        _ => None,
      })
  }

  /// The list `identity` as [`ContentStore::lists_in`] reads it, its bytes copied: none where it is no list kept here.
  fn list_now(&self, identity: &ContentId) -> Option<Box<[u8]>> {
    self
      .entries
      .get(identity)
      .filter(|held| held.form == Form::List)
      .and_then(Held::here)
      .map(Box::from)
  }

  /// The window `identity` let go, as [`ContentStore::aways_in`] reads it: its length, none where it is not let go.
  fn away_now(&self, identity: &ContentId) -> Option<u64> {
    match self.entries.get(identity).map(|held| &held.kept) {
      Some(Kept::Away { len }) => Some(u64::try_from(*len).unwrap_or(u64::MAX)),
      _ => None,
    }
  }

  /// Before the list `identity` is kept or freed: its value at the cut kept where its page is frozen (F2).
  fn before_list_change(&mut self, identity: ContentId) {
    let now = self.list_now(&identity);
    self.cut.lists.before(identity, || now, |bytes| bytes.len());
  }

  /// Before the name `identity` is made or dropped: its root at the cut kept where its page is frozen (F2).
  fn before_name_change(&mut self, identity: ContentId) {
    let now = self.objects.get(&identity).copied();
    self.cut.names.before(identity, || now, |_| IDENTITY_BYTES);
  }

  /// Before the window `identity` is let go, brought back or freed: whether it was let go at the cut, and its length,
  /// kept where its page is frozen (F2).
  fn before_away_change(&mut self, identity: ContentId) {
    let now = self.away_now(&identity);
    self
      .cut
      .aways
      .before(identity, || now, |_| core::mem::size_of::<u64>());
  }

  /// Freezes `family`'s coordinates `lo..=hi`, a page of the shard image's cut (A-870 piece 2, F2): an entry there
  /// keeps its value at the cut before it changes, until the page is thawed ([`ContentStore::thaw`]).
  pub fn freeze(&mut self, family: CutFamily, (lo, hi): (u64, u64)) {
    match family {
      CutFamily::Lists => self.cut.lists.freeze(lo, hi),
      CutFamily::Names => self.cut.names.freeze(lo, hi),
      CutFamily::Aways => self.cut.aways.freeze(lo, hi),
    }
  }

  /// Thaws `family`'s coordinates `lo..=hi`, a page of the cut now encoded: its pre-images dropped.
  pub fn thaw(&mut self, family: CutFamily, (lo, hi): (u64, u64)) {
    match family {
      CutFamily::Lists => self.cut.lists.thaw(lo, hi),
      CutFamily::Names => self.cut.names.thaw(lo, hi),
      CutFamily::Aways => self.cut.aways.thaw(lo, hi),
    }
  }

  /// The cut ended, its publication committed: every range thawed and pre-image dropped. How many were left (none
  /// where every page was thawed as it was encoded).
  pub fn end_cut(&mut self) -> usize {
    self
      .cut
      .lists
      .end()
      .saturating_add(self.cut.names.end())
      .saturating_add(self.cut.aways.end())
  }

  /// The pre-images kept since this was last asked, the counts started again.
  pub fn take_cut_counts(&mut self) -> PreimageCounts {
    self
      .cut
      .lists
      .take_counts()
      .and(self.cut.names.take_counts())
      .and(self.cut.aways.take_counts())
  }

  /// [`ContentStore::lists_in`] as at the cut: the lists now with their pre-images laid over them (F2), read until
  /// their bytes (an identity and the list's) pass `cap`; where the read stopped short of the range, the coordinate it
  /// stopped at (`crate::preimage::Preimages::overlay_within`).
  pub fn lists_at_cut(&self, (lo, hi): (u64, u64), cap: usize) -> CutLists {
    self.cut.lists.overlay_within(
      (lo, hi),
      self
        .lists_in(lo, hi)
        .map(|(identity, bytes)| (identity, Box::from(bytes))),
      (cap, &|_, bytes| IDENTITY_BYTES.saturating_add(bytes.len())),
    )
  }

  /// [`ContentStore::names_in`] as at the cut (F2), read until their bytes (two identities each) pass `cap`.
  pub fn names_at_cut(
    &self,
    (lo, hi): (u64, u64),
    cap: usize,
  ) -> (Vec<(ContentId, ContentId)>, Option<u64>) {
    self.cut.names.overlay_within(
      (lo, hi),
      self.names_in(lo, hi),
      (cap, &|_, _| IDENTITY_BYTES.saturating_mul(2)),
    )
  }

  /// [`ContentStore::aways_in`] as at the cut (F2), read until their bytes (an identity and a length each) pass `cap`.
  pub fn aways_at_cut(
    &self,
    (lo, hi): (u64, u64),
    cap: usize,
  ) -> (Vec<(ContentId, u64)>, Option<u64>) {
    self.cut.aways.overlay_within(
      (lo, hi),
      self.aways_in(lo, hi),
      (cap, &|_, _| IDENTITY_BYTES.saturating_add(size_of::<u64>())),
    )
  }

  /// The lists the store keeps, each with its bytes (A-727 slice a3b): what the shard image carries whole beside the
  /// windows, so an object longer than a window is whole again after a restart before any record is replayed.
  pub fn lists(&self) -> impl Iterator<Item = (ContentId, &[u8])> + '_ {
    self
      .entries
      .iter()
      .filter(|(_, held)| held.form == Form::List)
      .filter_map(|(identity, held)| Some((*identity, held.here()?)))
  }

  /// Each object longer than a window, by its identity, with the root this store cut it into (A-727 slice a3b).
  pub fn names(&self) -> impl Iterator<Item = (ContentId, ContentId)> + '_ {
    self
      .objects
      .iter()
      .map(|(identity, root)| (*identity, *root))
  }

  /// The lists and names kept or let go since the owner last took them ([`ContentStore::clear_changes`]): each list
  /// with its bytes, none once freed; each name with its root, none once unnamed (A-727 slice a3b: a delta carries them).
  pub fn changes(&self) -> Changes<'_> {
    let lists = self
      .lists_changed
      .iter()
      .map(|identity| {
        let bytes = self
          .entries
          .get(identity)
          .filter(|held| held.form == Form::List)
          .and_then(Held::here);
        (*identity, bytes)
      })
      .collect();
    let names = self
      .names_changed
      .iter()
      .map(|identity| (*identity, self.objects.get(identity).copied()))
      .collect();
    (lists, names)
  }

  /// Forgets the changes a committed publication carried.
  pub fn clear_changes(&mut self) {
    self.lists_changed.clear();
    self.names_changed.clear();
    self.away_changed.clear();
  }

  /// Every window let go ([`ContentStore::let_go`]) and not brought back, with its length: what a shard image carries
  /// whole, so a restart keeps each as let go (A-767 C1 step 2b, main's ruling (a) of 2026-11-22 02:52).
  pub fn aways(&self) -> impl Iterator<Item = (ContentId, u64)> + '_ {
    self
      .entries
      .iter()
      .filter_map(|(identity, held)| match held.kept {
        Kept::Away { len } => Some((*identity, u64::try_from(len).unwrap_or(u64::MAX))),
        _ => None,
      })
  }

  /// The windows let go, brought back or freed since the owner last took the changes: each with its length while it is
  /// let go, none once it is not (brought back, or freed): what a delta carries of [`ContentStore::aways`].
  pub fn away_changes(&self) -> Vec<(ContentId, Option<u64>)> {
    self
      .away_changed
      .iter()
      .map(|identity| {
        let away = match self.entries.get(identity).map(|held| &held.kept) {
          Some(Kept::Away { len }) => Some(u64::try_from(*len).unwrap_or(u64::MAX)),
          _ => None,
        };
        (*identity, away)
      })
      .collect()
  }

  /// Takes back the windows let go that a shard image carried ([`ContentStore::aways`]), before its lists
  /// ([`ContentStore::restore_nodes`], which hold the nodes they name): each an entry of no bytes and its length, landed,
  /// held by no one until a list or a row holds it again, read as nothing until brought back by its own bytes. One the
  /// store holds already, or of no length or past a window, is left out: the image recorded it, so the store never
  /// infers one (a window the image never named stays absent, a reader's typed `NotHeld`). The count left out.
  pub fn restore_away(&mut self, aways: Vec<(ContentId, u64)>) -> u64 {
    let mut refused = 0u64;
    for (identity, len) in aways {
      let len = usize::try_from(len)
        .ok()
        .filter(|len| *len > 0 && *len <= self.window.get());
      let Some(len) = len.filter(|_| !self.entries.contains_key(&identity)) else {
        refused = refused.saturating_add(1);
        continue;
      };
      self.entries.insert(
        identity,
        Held {
          kept: Kept::Away { len },
          form: Form::Window,
          holders: 0,
          landed: true,
          unlanded: 0,
          last_read: core::cell::Cell::new(0),
        },
      );
    }
    refused
  }

  /// Takes back the lists and names a shard image carried (A-727 slice a3b), after the windows its storage restored
  /// ([`ContentStore::with_storage`]): each list held by no one until a holder puts or holds its object again, holding
  /// once each node it names, as it did; each name whose root the store holds. A list whose bytes are not its identity's
  /// or not a list's, or that names a node the store does not hold, and a name whose root it does not hold, are left
  /// out. The count left out.
  pub fn restore_nodes(
    &mut self,
    lists: Vec<(ContentId, Vec<u8>)>,
    names: Vec<(ContentId, ContentId)>,
  ) -> u64 {
    let mut refused = 0u64;
    let mut kept = Vec::new();
    for (identity, bytes) in lists {
      let whole = ContentId::named(Form::List, &bytes) == identity
        && bytes.len() % IDENTITY_BYTES == 0
        && !bytes.is_empty()
        && !self.entries.contains_key(&identity);
      if !whole {
        refused = refused.saturating_add(1);
        continue;
      }
      self.content_bytes = self
        .content_bytes
        .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
      self.entries.insert(
        identity,
        Held {
          kept: Kept::Here(bytes.into_boxed_slice()),
          form: Form::List,
          holders: 0,
          landed: false,
          unlanded: 0,
          last_read: core::cell::Cell::new(0),
        },
      );
      kept.push(identity);
    }
    // Each list holds the nodes it names, once for each time it names them, as it did before the restart.
    for identity in kept {
      let named: Vec<ContentId> = self
        .entries
        .get(&identity)
        .and_then(Held::here)
        .map(|bytes| {
          bytes
            .as_chunks::<IDENTITY_BYTES>()
            .0
            .iter()
            .filter_map(|chunk| ContentId::from_slice(chunk))
            .collect()
        })
        .unwrap_or_default();
      for node in named {
        match self.entries.get_mut(&node) {
          Some(held) => held.holders = held.holders.saturating_add(1),
          None => refused = refused.saturating_add(1),
        }
      }
    }
    for (identity, root) in names {
      if self.entries.contains_key(&root) && !self.objects.contains_key(&identity) {
        self.objects.insert(identity, root);
        self.rooted.insert(root, identity);
      } else {
        refused = refused.saturating_add(1);
      }
    }
    refused
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

  /// The bytes the store holds under the content `identity` names, its whole tree: each window and each list naming
  /// them, as held now (an object one window long, that window alone). What letting it go gives back where no other
  /// content shares a node of it; [`ContentStore::len_of`] is the root node's alone (main 01a20182's ruling of
  /// 2026-11-24: a git fragment kept as a tree was weighed at its root's length, a few dozen bytes).
  pub fn tree_bytes(&self, identity: &ContentId) -> u64 {
    let mut total = 0_u64;
    let mut pending = vec![self.resolve(identity)];
    while let Some(node) = pending.pop() {
      let Some(held) = self.entries.get(&node) else {
        continue;
      };
      total = total.saturating_add(u64::try_from(held.len()).unwrap_or(u64::MAX));
      if held.form == Form::List
        && let Some(list) = held.here()
      {
        pending.extend(
          list
            .as_chunks::<IDENTITY_BYTES>()
            .0
            .iter()
            .copied()
            .map(ContentId),
        );
      }
    }
    total
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

  /// The bytes of the object `identity` names (its whole length `len`) from `offset` to the end of the store's window
  /// holding `offset`: a read's next run (A-727; w-reaper's referenced files, A-645 P3; the svn reads), lent where the
  /// storage lends its window (a chunk in the clear: nothing copied, nothing allocated). None at or past `len`, for a
  /// length the store can tell is not the object's, or where the store does not hold the path down whole.
  pub fn run_at(&self, identity: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>> {
    if offset >= len {
      return None;
    }
    // `len` is the object's whole length, as its holder records it: the tree a window is found down is cut from it.
    // What the store can check of it, it does: an object of one window is that window, its length the window's; a
    // longer one is a tree, its length past one window. A wrong length would find another tree's windows: refused.
    let window = u64::try_from(self.window.get()).ok()?;
    let fits = match self.form(identity)? {
      Form::Window => self.len_of(identity) == Some(len),
      Form::List => len > window,
      Form::Table | Form::Derived => false,
    };
    if !fits {
      return None;
    }
    let index = offset.checked_div(window)?;
    let start = index.saturating_mul(window);
    let from = usize::try_from(offset.saturating_sub(start)).ok()?;
    match self.window_at(identity, len, index)? {
      Cow::Borrowed(bytes) => bytes.get(from..).map(Cow::Borrowed),
      Cow::Owned(bytes) => bytes.get(from..).map(|rest| Cow::Owned(rest.to_vec())),
    }
  }

  /// The window `index` of an object `len` bytes long whose root is `root` ([`ContentStore::put_object`]), read down
  /// the one list a level that names it (A-639): the object was cut into windows of this store's window and lists of
  /// its fanout, so the list naming window `index` at each level is that level's digit of `index` in the fanout's base.
  /// A reader that resumes at a window reads the lists above it alone, never the windows before it. `None` past the
  /// object's last window, or where the store does not hold the path down whole.
  pub fn window_at(&self, root: &ContentId, len: u64, index: u64) -> Option<Cow<'_, [u8]>> {
    self.window_at_in(root, len, (index, &()))
  }

  /// Copies `into.len()` bytes of window `index` of the object `root` (`len` bytes long) from `offset` in the window
  /// into `into`, its storage reading from `kept_in` where none is lent to it now (A-760: a named run read a slice a
  /// step, no more of the window copied). None past the window's bytes or for a window not held.
  pub fn window_range_in(
    &self,
    (root, len, index): (&ContentId, u64, u64),
    (offset, into): (usize, &mut [u8]),
    kept_in: &dyn std::any::Any,
  ) -> Option<()> {
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
    self.note(&identity, held);
    match held_window(held) {
      Some(stored) => self.storage.read_range_in(stored, (offset, into), kept_in),
      None => {
        let here = held.here()?;
        into.copy_from_slice(here.get(offset..offset.checked_add(into.len())?)?);
        Some(())
      }
    }
  }

  /// [`ContentStore::window_at`], its storage reading from `kept_in` where none is lent to it now
  /// ([`WindowStorage::read_in`], A-760).
  pub fn window_at_in<'a>(
    &'a self,
    root: &ContentId,
    len: u64,
    (index, kept_in): (u64, &'a dyn std::any::Any),
  ) -> Option<Cow<'a, [u8]>> {
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
    self.note(&identity, held);
    match held_window(held) {
      Some(window) => self.storage.read_in(window, kept_in),
      None => held.here().map(Cow::Borrowed),
    }
  }

  /// The window of the object `root` (`len` bytes long) that holds byte `offset`, where it is let go
  /// ([`ContentStore::let_go`]): its identity and its range in the object, for a reader that met it to have its bytes
  /// brought back ([`ContentStore::bring_back`]) before it reads on. `None` where that window is held, or the store does
  /// not hold the path down to it.
  pub fn away_window(
    &self,
    root: &ContentId,
    len: u64,
    offset: u64,
  ) -> Option<(ContentId, u64, u64)> {
    let root = &self.resolve(root);
    let window = u64::try_from(self.window.get()).ok()?;
    let fanout = u64::try_from(self.fanout.get()).ok()?;
    let windows = len.div_ceil(window).max(1);
    let index = offset.checked_div(window)?;
    if index >= windows {
      return None;
    }
    let height = list_levels(windows, fanout)?;
    let identity = self.window_identity(root, (height, fanout), index)?;
    let Kept::Away { len: away } = self.entries.get(&identity)?.kept else {
      return None;
    };
    let start = index.checked_mul(window)?;
    Some((identity, start, u64::try_from(away).ok()?))
  }

  /// The window holding byte `offset` of the object `root` names (`len` bytes long), let go or held: its identity and its
  /// range in the object, for a reader that keeps it while it reads (A-760 (b) 4d). `None` past the object's end, or
  /// where the store does not hold the path down to it.
  pub fn window_holding(
    &self,
    root: &ContentId,
    len: u64,
    offset: u64,
  ) -> Option<(ContentId, u64, u64)> {
    let root = &self.resolve(root);
    let window = u64::try_from(self.window.get()).ok()?;
    let fanout = u64::try_from(self.fanout.get()).ok()?;
    let windows = len.div_ceil(window).max(1);
    let index = offset.checked_div(window)?;
    if index >= windows {
      return None;
    }
    let height = list_levels(windows, fanout)?;
    let identity = self.window_identity(root, (height, fanout), index)?;
    let start = index.checked_mul(window)?;
    Some((identity, start, len.saturating_sub(start).min(window)))
  }

  /// The identity (the plain BLAKE3 of its bytes) of the object this store keeps as `root`: the name the node's store
  /// keys it by. A root that is a window is its own identity.
  pub fn object_identity(&self, root: &ContentId) -> ContentId {
    self.rooted.get(root).copied().unwrap_or(*root)
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

  /// The windows of the object `identity` names, in order, each with its offset in the object (itself at 0 where it is
  /// one window), let go or not: where a home that holds the object finds each window in the pack it landed in (goal 93
  /// item 2b(b)).
  pub fn object_layout(&self, identity: &ContentId) -> Vec<(ContentId, u64)> {
    let mut offset = 0u64;
    self
      .object_windows(identity)
      .into_iter()
      .map(|window| {
        let at = offset;
        offset = offset.saturating_add(self.len_of(&window).unwrap_or(0));
        (window, at)
      })
      .collect()
  }

  /// Whether the store holds the object `identity` names whole in memory: held, and none of its windows let go
  /// ([`ContentStore::let_go`]). A reader that needs its bytes and finds it not whole reads it again from where it was
  /// landed, and its put brings the windows let go back.
  pub fn holds_whole(&self, identity: &ContentId) -> bool {
    self.contains(identity)
      && self
        .object_windows(identity)
        .iter()
        .all(|window| !self.is_away(window))
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

  /// What the store holds, by where its bytes are and what they are (the memory status's gauges, main's ruling of
  /// 2026-11-22 03:46): a walk of every entry, so only a memory status asks.
  pub fn census(&self) -> ContentCensus {
    let mut census = ContentCensus {
      objects: u64::try_from(self.objects.len()).unwrap_or(u64::MAX),
      used: self.used(),
      charged: self.charged_bytes,
      ..ContentCensus::default()
    };
    for held in self.entries.values() {
      let len = u64::try_from(held.len()).unwrap_or(u64::MAX);
      let (count, bytes) = match (&held.kept, held.form) {
        (Kept::Away { .. }, _) => (&mut census.away, &mut census.away_bytes),
        (Kept::Stored { .. }, _) => (&mut census.stored, &mut census.stored_bytes),
        (Kept::Here(_), Form::Window) => (&mut census.here_windows, &mut census.here_window_bytes),
        (Kept::Here(_), Form::List) => (&mut census.lists, &mut census.list_bytes),
        (Kept::Here(_), Form::Table) => (&mut census.tables, &mut census.table_bytes),
        (Kept::Here(_), Form::Derived) => (&mut census.derived, &mut census.derived_bytes),
      };
      *count = count.saturating_add(1);
      *bytes = bytes.saturating_add(len);
    }
    census
  }

  /// What the store has done.
  pub fn counters(&self) -> ContentCounters {
    self.counters
  }

  /// The reads so far of a content whose name the store keeps, each a content's bytes or a window of an object, let go
  /// or not (a let-go window's read finds no bytes): the clock its coldest windows are found by. A read of a name it
  /// does not keep is not counted.
  pub fn reads(&self) -> u64 {
    self.reads.get()
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

  /// The next window's identity, its lists walked through and none read: what [`ContentStore::let_go_object`] lets
  /// go; `None` once every list is read, or at a content the store does not hold.
  fn next_window(&mut self) -> Option<ContentId> {
    loop {
      let identity = self.next_identity()?;
      let held = self.store.entries.get(&identity)?;
      if held.form != Form::List {
        return Some(identity);
      }
      self.stack.push((held.here()?, 0));
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
          Kept::Away { .. } => None,
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
  /// deduplicated puts counted, and the content freed only with its third release, answered freed then.
  #[test]
  fn the_same_bytes_put_three_times_are_one_content_freed_with_its_last_holder() {
    let mut store = ContentStore::new(1 << 20, 1 << 10).unwrap();
    let bytes: &[u8] = b"fn main() {}\n";
    let first = put_three_times(&mut store, bytes);
    assert_eq!(store.release(&first).unwrap(), Freed::Held);
    assert_eq!(store.release(&first).unwrap(), Freed::Held);
    assert_eq!(store.bytes_of(&first).as_deref(), Some(bytes));
    assert_eq!(
      store.release(&first).unwrap(),
      Freed::Gone,
      "freed with its last holder"
    );
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

  /// The guard on room asked inside a lend (main 01a20182's ruling of 2026-11-23 21:30). Do: set a budget out of reach
  /// with room left; put new bytes, charge, put bytes held already, release; set the budget again and put. Expect: the
  /// put and the charge refused `OutOfReach` with their need, whatever room is left, nothing changed; the put of held
  /// bytes (it needs none) and the release go on; once the budget is set again the put is taken.
  #[test]
  fn a_budget_out_of_reach_refuses_every_byte_by_its_own_kind_until_it_is_set_again() {
    let mut store = ContentStore::new(64, NARROW).unwrap();
    let held = store.put(b"held").unwrap();
    store.set_budget_out_of_reach(64);
    assert!(store.room() > 3, "room left, so the kind alone refuses");
    assert_eq!(
      store.put(b"new"),
      Err(ContentRefused::OutOfReach { need: 3 })
    );
    assert_eq!(store.charge(1), Err(ContentRefused::OutOfReach { need: 1 }));
    assert_eq!((store.len(), store.used()), (1, 4));
    assert_eq!(store.put(b"held"), Ok(held));
    assert_eq!(store.release(&held), Ok(Freed::Held));
    store.set_budget(64);
    assert!(store.put(b"new").is_ok());
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

  thread_local! {
    /// The windows [`CountingWindows`] read out on this thread.
    static WINDOW_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
  }

  /// A storage that counts the windows read out of it (goal 93, P7 finding (2): what a release reads back).
  #[derive(Debug, Default)]
  struct CountingWindows {
    inner: HeapWindows,
  }

  impl WindowStorage for CountingWindows {
    fn put(&mut self, bytes: &[u8]) -> Result<StoredWindow, ContentRefused> {
      self.inner.put(bytes)
    }

    fn read(&self, window: StoredWindow) -> Option<Cow<'_, [u8]>> {
      WINDOW_READS.with(|reads| reads.set(reads.get() + 1));
      self.inner.read(window)
    }

    fn release(&mut self, window: StoredWindow) {
      self.inner.release(window);
    }
  }

  /// Goal 93, P7 finding (2) (main 01a20182's routing of 2026-11-22 ~14:20: `release_one`'s copy, 11.5 MB of the import's
  /// sampled allocations). Do: in a store whose windows a storage keeps, put an object of five windows twice, release
  /// it twice. Expect: the first release frees nothing and reads nothing; the second frees the object (answered true),
  /// every list and window it held, and reads no window back out of the storage: a freed window's bytes are no one's.
  /// Failing first: each freed window was read out and copied into a buffer only `release`'s answer carried, which no
  /// caller used.
  #[test]
  fn a_released_objects_windows_are_freed_without_being_read_back() {
    let reads = || WINDOW_READS.with(std::cell::Cell::get);
    let mut store =
      ContentStore::with_storage(1 << 20, NARROW, Box::new(CountingWindows::default())).unwrap();
    let bytes: Vec<u8> = (0..5 * NARROW)
      .map(|at| u8::try_from(at % 251).unwrap())
      .collect();
    let root = store.put_object(&bytes).unwrap();
    assert_eq!(store.put_object(&bytes).unwrap(), root);
    let before = reads();
    assert_eq!(
      store.release(&root).unwrap(),
      Freed::Held,
      "freed with a holder left"
    );
    assert_eq!(
      store.release(&root).unwrap(),
      Freed::Gone,
      "not freed with its last holder"
    );
    assert_eq!(reads() - before, 0, "windows read back as they were freed");
    assert_eq!(store.len(), 0, "something of the object is still held");
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
    let bytes: Vec<u8> = (0..7 * NARROW + 5)
      .map(|at| u8::try_from(at * 7 % 251).unwrap())
      .collect();
    for len in [0, 1, NARROW - 1, NARROW, NARROW + 1, bytes.len()] {
      let object = &bytes[..len];
      for piece in [1, 3, 61, len.max(1)] {
        assert_put_by_pieces_is_put_whole(object, piece);
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

  /// Goal 93, svn finding (2) (main 01a20182, 2026-11-22 03:46 PST; Ada's rule: what the design can avoid, it does not
  /// allocate). Do: begin an object of half a window, and one of three windows and a half, each with its length, and
  /// put it in pieces of a quarter window. Expect: the window's buffer reserved once, at the begin, at the window or the
  /// object's length if shorter, never grown or moved by a piece; the object the one put whole. Failing first: the
  /// buffer grew by doubling from the first piece, so a 256 KiB text put in 65 KB copy slices made allocations of 65,
  /// 130, 260 and 520 KB: 97.5 MB for 26.2 MB of text, 400 allocations, in w-svn's attribution
  /// (`svn_commit::store_step_from`).
  #[test]
  fn an_object_begun_with_its_length_reserves_its_windows_buffer_once() {
    for len in [NARROW / 2, 3 * NARROW + NARROW / 2] {
      let bytes: Vec<u8> = (0..len)
        .map(|at| u8::try_from(at * 7 % 251).unwrap())
        .collect();
      let mut whole = ContentStore::new(1 << 20, NARROW).unwrap();
      let mut streamed = ContentStore::new(1 << 20, NARROW).unwrap();
      let mut writing = streamed.begin_object_of_length(u64::try_from(len).unwrap());
      let reserved = (writing.pending.capacity(), writing.pending.as_ptr());
      assert_eq!(reserved.0, len.min(NARROW), "{len}: reserved at the begin");
      for piece in bytes.chunks(NARROW / 4) {
        streamed.put_window(&mut writing, piece).unwrap();
        assert_eq!(
          (writing.pending.capacity(), writing.pending.as_ptr()),
          reserved,
          "{len}: a piece neither grows nor moves the window's buffer"
        );
      }
      assert_eq!(
        streamed.finish_object(&mut writing).unwrap(),
        whole.put_object(&bytes).unwrap(),
        "{len}"
      );
      assert_eq!(held(&whole), held(&streamed), "{len}");
    }
  }

  /// Goal 93, P7 finding (2) (main 01a20182's routing of 2026-11-22 07:38: the import's sink made one window buffer a
  /// text). Do: put four objects in turn, each begun on the buffer the one before handed back (`take_buffer`), the first
  /// on an empty one: half a window, three windows and a half, a quarter window, and one of unknown length, in pieces of
  /// a quarter window, the third abandoned part-way. Expect: the buffer grown once, at the first object, to the window,
  /// and never grown or moved after; every finished object the one put whole; the abandoned one leaving nothing held.
  /// Failing first: each object made a buffer of its own.
  #[test]
  fn objects_begun_in_turn_on_one_buffer_never_make_another() {
    let mut whole = ContentStore::new(1 << 20, NARROW).unwrap();
    let mut streamed = ContentStore::new(1 << 20, NARROW).unwrap();
    let mut buffer = Vec::new();
    let mut seen: Option<(usize, *const u8)> = None;
    for (at, len) in [NARROW / 2, 3 * NARROW + NARROW / 2, NARROW / 4, 2 * NARROW]
      .into_iter()
      .enumerate()
    {
      let bytes: Vec<u8> = (0..len)
        .map(|byte| u8::try_from((byte * 7 + at) % 251).unwrap())
        .collect();
      let known = (at != 3).then(|| u64::try_from(len).unwrap());
      let mut writing = streamed.begin_object_reusing(known, std::mem::take(&mut buffer));
      let now = (writing.pending.capacity(), writing.pending.as_ptr());
      assert_eq!(now.0, NARROW, "object {at}: the buffer is the window's");
      if let Some(seen) = seen {
        assert_eq!(
          now, seen,
          "object {at}: the same buffer, neither grown nor moved"
        );
      }
      seen = Some(now);
      if at == 2 {
        streamed.put_window(&mut writing, &bytes[..1]).unwrap();
        buffer = writing.take_buffer();
        streamed.abandon_object(writing);
        continue;
      }
      for piece in bytes.chunks(NARROW / 4) {
        streamed.put_window(&mut writing, piece).unwrap();
      }
      assert_eq!(
        streamed.finish_object(&mut writing).unwrap(),
        whole.put_object(&bytes).unwrap(),
        "object {at}"
      );
      buffer = writing.take_buffer();
    }
    assert_eq!(
      held(&whole),
      held(&streamed),
      "the objects finished, nothing of the abandoned one"
    );
  }

  /// `object` put into one store whole and into another `piece` bytes at a time, twice each: the same root, the same
  /// contents and bytes used, and the object read back.
  fn assert_put_by_pieces_is_put_whole(object: &[u8], piece: usize) {
    let len = object.len();
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
      streamed
        .object_bytes(&root, u64::try_from(len).unwrap())
        .as_deref(),
      Some(object),
      "{len} by {piece}"
    );
  }

  /// A-727 slice a3b (an object longer than a window across a restart). Do: put two objects of five windows into a
  /// store; carry its windows (by identity and bytes), its lists and its names into a new store, as a shard image does;
  /// read both objects by identity before anything is put; put the first again; free what no one holds. Expect: both
  /// read whole by their identities before any put; the put again storing no window and no list (the lists restored
  /// answer it) and holding the object once; the second object, held by no one, freed with every window and list it
  /// named, the first whole.
  #[test]
  fn an_object_past_a_window_is_whole_again_after_a_restart_from_its_lists_and_name() {
    let window = 2 * IDENTITY_BYTES;
    let first: Vec<u8> = (0..5 * window)
      .map(|at| u8::try_from(at % 199).unwrap())
      .collect();
    let second: Vec<u8> = (0..5 * window)
      .map(|at| u8::try_from(at % 97).unwrap())
      .collect();
    let mut before = ContentStore::new(1 << 20, window).unwrap();
    let (one, two) = (
      before.put_object(&first).unwrap(),
      before.put_object(&second).unwrap(),
    );
    let storage = kept_windows(&before);
    let lists: Vec<(ContentId, Vec<u8>)> = before
      .lists()
      .map(|(identity, bytes)| (identity, bytes.to_vec()))
      .collect();
    let names: Vec<(ContentId, ContentId)> = before.names().collect();
    assert!(!lists.is_empty() && names.len() == 2);
    let mut after = ContentStore::with_storage(1 << 20, window, Box::new(storage)).unwrap();
    assert_eq!(after.restore_nodes(lists, names), 0);
    let len = u64::try_from(first.len()).unwrap();
    assert_eq!(after.object_bytes(&one, len), Some(first.clone()));
    assert_eq!(after.object_bytes(&two, len), Some(second.clone()));
    assert_put_again_answered_by_the_restored(&mut after, &first, one);
    let freed = after.release_unheld();
    assert!(freed > 0, "the second object's nodes freed");
    assert_eq!(after.object_bytes(&two, len), None);
    assert_eq!(after.object_bytes(&one, len), Some(first));
    after.release(&one).unwrap();
    assert!(after.is_empty() && after.used() == 0);
  }

  /// A storage that kept every window `before` holds, as one kept across a restart.
  fn kept_windows(before: &ContentStore) -> RestoredWindows {
    let mut storage = RestoredWindows::default();
    for (identity, held) in &before.entries {
      if held.form == Form::Window {
        let bytes = before.bytes_of(identity).unwrap().into_owned();
        let kept = storage.inner.put(&bytes).unwrap();
        storage.kept.push((*identity, kept, bytes.len()));
      }
    }
    storage
  }

  /// `first` put again into the restored store `after` is answered by the nodes kept (nothing new stored), held once.
  fn assert_put_again_answered_by_the_restored(
    after: &mut ContentStore,
    first: &[u8],
    one: ContentId,
  ) {
    let stored = after.counters().stored;
    assert_eq!(after.put_object(first).unwrap(), one);
    assert_eq!(
      after.counters().stored,
      stored,
      "the restored nodes answer the put"
    );
    assert_eq!(after.holders(&one), 1);
  }

  /// A-724 piece 4 (§4.17.5 "The cold trigger": a window past its repository's grant let go to the node's store). Do:
  /// put a window, try to let it go before it is marked landed, mark it, let it go, read it, bring it back with other
  /// bytes and then with its own. Expect: refused `NotLanded` first; then freed (its bytes leave the store's count, its
  /// length stays), read as nothing, counted; other bytes refused; its own bytes bring it back, read whole, counted.
  #[test]
  fn a_landed_window_is_let_go_and_brought_back_by_its_own_bytes() {
    let mut store = ContentStore::new(1 << 20, 4096).unwrap();
    let bytes = vec![7u8; 1000];
    let id = store.put(&bytes).unwrap();
    let before = let_go_once_landed(&mut store, &id);
    brought_back_by_its_own_bytes(&mut store, (&id, &bytes), before);
  }

  /// The window `id` refused `NotLanded`, then marked and let go: freed, read as nothing, its length kept, counted, a
  /// second let-go refused `Away`. The store's count before it was let go.
  fn let_go_once_landed(store: &mut ContentStore, id: &ContentId) -> u64 {
    assert_eq!(store.let_go(id), Err(LetGoRefused::NotLanded));
    assert!(store.mark_landed(id));
    let before = store.content_bytes;
    assert_eq!(store.let_go(id), Ok(1000));
    assert_eq!(store.content_bytes, before - 1000);
    assert!(store.is_away(id));
    assert_eq!(store.bytes_of(id), None);
    assert_eq!(store.len_of(id), Some(1000));
    assert_eq!(store.let_go(id), Err(LetGoRefused::Away));
    assert_eq!(store.counters.let_go, 1);
    before
  }

  /// The window `id`, let go, refused other bytes and brought back by `bytes`, its own: read whole, counted, the store's
  /// count `before` again.
  fn brought_back_by_its_own_bytes(
    store: &mut ContentStore,
    (id, bytes): (&ContentId, &[u8]),
    before: u64,
  ) {
    assert_eq!(
      store.bring_back(id, &vec![8u8; 1000]),
      Err(ContentRefused::NotHeld)
    );
    store.bring_back(id, bytes).unwrap();
    assert!(!store.is_away(id));
    assert_eq!(store.bytes_of(id).as_deref(), Some(bytes));
    assert_eq!(store.counters.brought_back, 1);
    assert_eq!(store.content_bytes, before);
  }

  /// `len` bytes of a pattern that repeats every 251, so no two windows of a test's object are alike.
  fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|at| u8::try_from(at % 251).unwrap()).collect()
  }

  /// Goal 93, main's ruling (B) of 2026-11-24 (one rule at the one let-go every path uses: a landed object's let-go never
  /// takes a window an unlanded object names). Do: put a landed object A of two windows and an unlanded object B whose
  /// first window is A's; pin B unlanded; let A go as a landed segment's let-go does (whatever its holders), and as the
  /// cold trigger does; then unpin B and let A go again. Expect, pinned: A's second window let go, the shared one kept,
  /// counted (`kept_unlanded`), every let-go of it refused `Unlanded`; unpinned: the shared window let go.
  #[test]
  fn a_window_an_unlanded_object_names_is_never_let_go_until_it_is_unpinned() {
    let window = 4096usize;
    let mut store = ContentStore::new(1 << 22, window).unwrap();
    let shared = patterned(window);
    let a_bytes = [&shared[..], &patterned(window)[1..], b"a"].concat();
    let b_bytes = [&shared[..], b"the rest of b".as_slice()].concat();
    let a = store.put_object(&a_bytes).unwrap();
    let b = store.put_object(&b_bytes).unwrap();
    let shared_id = ContentId::of(&shared);
    let a_second = ContentId::of(a_bytes.get(window..window * 2).unwrap());
    assert_eq!(store.pin_unlanded(&b), Ok(2), "B's two windows pinned");
    let before = store.counters().kept_unlanded;
    let (freed, kept) = store.let_go_object_landed(&a, &|_| None);
    assert!(
      freed > 0 && kept == 1,
      "A's own window let go, the shared one kept: {freed} {kept}"
    );
    assert!(store.is_away(&a_second), "A's own window let go");
    assert!(!store.is_away(&shared_id), "the window B names kept");
    assert_eq!(store.let_go(&shared_id), Err(LetGoRefused::Unlanded));
    assert!(
      store.counters().kept_unlanded > before,
      "the kept window counted"
    );
    store.unpin_unlanded(&b);
    let (freed, _) = store.let_go_object_landed(&a, &|_| None);
    assert!(freed > 0, "unpinned, the shared window let go");
    assert!(
      store.is_away(&shared_id),
      "the shared window let go once unpinned"
    );
  }

  /// Goal 93, ruling (B), w-perf's review: a pin that finds its object's bytes already gone is a named bug signal, and a
  /// pin of nothing is refused. Do: let one window of a two-window object go, then pin the object; pin an object the store
  /// never held. Expect: the object pinned whole, the away window counted (`pinned_away` 1), and a put of its bytes
  /// bringing it back still pinned (its let-go refused `Unlanded`); the unheld object refused `NotHeld`, nothing pinned.
  #[test]
  fn a_pin_that_finds_a_window_let_go_counts_it_and_a_pin_of_nothing_is_refused() {
    let window = 4096usize;
    let mut store = ContentStore::new(1 << 22, window).unwrap();
    let bytes = patterned(window * 2);
    let object = store.put_object(&bytes).unwrap();
    let second = ContentId::of(bytes.get(window..).unwrap());
    let_go_each(&mut store, &[second]);
    assert_eq!(store.pin_unlanded(&object), Ok(2), "both windows pinned");
    assert_eq!(store.counters().pinned_away, 1, "the away window counted");
    store.put(bytes.get(window..).unwrap()).unwrap();
    assert!(!store.is_away(&second), "the put brought it back");
    assert_eq!(store.let_go(&second), Err(LetGoRefused::Unlanded));
    let never = ContentId::of(b"never held");
    assert_eq!(store.pin_unlanded(&never), Err(ContentRefused::NotHeld));
    assert_eq!(store.counters().pinned_away, 1, "nothing more pinned");
  }

  /// A-767 C1 step 2a (a push's work reading a text an import let go). Do: put an object of three windows and a part,
  /// mark each window landed and let it go, then bring the object back from bytes not its own, and from its own. Expect:
  /// every window away and its bytes out of the store's count; other bytes refused `NotHeld`, nothing brought back; its
  /// own bytes bring back all four windows, each read whole, the count as before, and a second bring-back brings none.
  #[test]
  fn an_object_let_go_is_brought_back_whole_by_its_own_bytes() {
    let window = 4096usize;
    let mut store = ContentStore::new(1 << 22, window).unwrap();
    let bytes = patterned(window * 3 + 100);
    let object = store.put_object(&bytes).unwrap();
    let before = store.content_bytes;
    let windows: Vec<ContentId> = bytes.chunks(window).map(ContentId::of).collect();
    let_go_each(&mut store, &windows);
    assert!(
      store.content_bytes < before,
      "the windows' bytes left the count"
    );
    let mut other = bytes.clone();
    other[0] ^= 1;
    assert_eq!(
      store.bring_back_object(&object, &other),
      Err(ContentRefused::NotHeld)
    );
    assert!(
      windows.iter().all(|id| store.is_away(id)),
      "nothing brought back by other bytes"
    );
    brought_back_whole(&mut store, (&object, &bytes), (&windows, window), before);
  }

  /// A-767 C5 and A-863. A recovery has a pack's lists and the lengths of its released
  /// windows. Put the bytes read from disk as Git's replay of a changeset read from the node's store puts them
  /// (`rows::put_whole`), then read the pack by identity. Both the one-window pack and the two-window pack must read
  /// whole.
  #[test]
  fn putting_a_payload_read_from_disk_restores_every_window_after_recovery() {
    for len in [NARROW, NARROW + 1] {
      let bytes = patterned(len);
      let mut before = ContentStore::new(u64::MAX, NARROW).unwrap();
      let object = crate::rows::put(&mut before, &bytes).unwrap();
      let byte_count = u64::try_from(len).unwrap();
      assert_eq!(
        before.let_go_object_landed(&object, &|_| None),
        (byte_count, 0)
      );
      let mut after = ContentStore::new(u64::MAX, NARROW).unwrap();
      assert_eq!(after.restore_away(before.aways().collect()), 0);
      let lists = before
        .lists()
        .map(|(id, list)| (id, list.to_vec()))
        .collect();
      assert_eq!(after.restore_nodes(lists, before.names().collect()), 0);
      assert!(after.object_bytes(&object, byte_count).is_none());
      let put = crate::rows::put_whole(&mut after, &bytes).unwrap();
      assert_eq!(put, object);
      assert_eq!(
        after
          .object_bytes(&put, byte_count)
          .map(|read| read == bytes),
        Some(true),
        "a successful put must return a readable {len}-byte payload"
      );
    }
  }

  /// Each of `windows` marked landed and let go.
  fn let_go_each(store: &mut ContentStore, windows: &[ContentId]) {
    for id in windows {
      assert!(store.mark_landed(id));
      store.let_go(id).unwrap();
      assert!(store.is_away(id));
    }
  }

  /// `object` brought back by `bytes`, its own: every one of its `windows` (`window` bytes each) read whole, the store's
  /// count `before` again, and a second bring-back bringing none.
  fn brought_back_whole(
    store: &mut ContentStore,
    (object, bytes): (&ContentId, &[u8]),
    (windows, window): (&[ContentId], usize),
    before: u64,
  ) {
    assert_eq!(
      store.bring_back_object(object, bytes),
      Ok(u64::try_from(windows.len()).unwrap())
    );
    for (id, piece) in windows.iter().zip(bytes.chunks(window)) {
      assert_eq!(store.bytes_of(id).as_deref(), Some(piece));
    }
    assert_eq!(store.content_bytes, before);
    assert_eq!(
      store.bring_back_object(object, bytes),
      Ok(0),
      "held already: none again"
    );
  }

  /// A-767 C1 step 2b. Do: put an object of three windows and a part, and a window another holder keeps as well; let go
  /// each object; then bring the first back. Expect: the object's four windows let go, their bytes freed and out of the
  /// count, the store's lists kept; the shared window kept whole; the first object brought back whole by its own bytes.
  #[test]
  fn an_object_is_let_go_window_by_window_but_what_another_holds() {
    let window = 4096usize;
    let mut store = ContentStore::new(1 << 22, window).unwrap();
    let bytes = patterned(window * 3 + 100);
    let object = store.put_object(&bytes).unwrap();
    let shared = store.put_object(&[9u8; 300]).unwrap();
    store.hold(&shared).unwrap();
    let before = store.content_bytes;
    assert_eq!(
      store.let_go_object(&object),
      u64::try_from(bytes.len()).unwrap()
    );
    assert_eq!(
      store.content_bytes,
      before - u64::try_from(bytes.len()).unwrap()
    );
    assert!(
      bytes
        .chunks(window)
        .all(|piece| store.is_away(&ContentId::of(piece)))
    );
    assert_eq!(
      store.let_go_object(&shared),
      0,
      "a window another holds stays"
    );
    assert!(!store.is_away(&shared));
    assert_eq!(store.bring_back_object(&object, &bytes), Ok(4));
    assert_eq!(
      store.object_bytes(&object, u64::try_from(bytes.len()).unwrap()),
      Some(bytes)
    );
  }

  /// Goal 93 item 2b (main 01a20182's ruling of 2026-11-22 06:58: a landed window may go whatever its holders count, but
  /// only where every holder of it can bring it back). Do: put an object of three windows and a part, hold each window
  /// twice more (a landed push's holders: its staged pack, its repository's), then let it go landed with the second window named
  /// pinned by a home; then let it go landed with none pinned; bring every window back by the object's bytes. Expect:
  /// `let_go` refuses each `Shared`; the first landed let-go frees every window but the pinned one, which is refused
  /// `Pinned { by: "homes" }` and stays; the second frees that one too; the object read whole once brought back.
  #[test]
  fn a_landed_window_goes_whatever_its_holders_unless_a_holder_pins_it() {
    let window = 4096usize;
    let mut store = ContentStore::new(1 << 22, window).unwrap();
    let bytes = patterned(window * 3 + 100);
    let object = store.put_object(&bytes).unwrap();
    let windows = landed_with_two_more_holders(&mut store, &bytes, window);
    let pinned = windows[1];
    assert_eq!(
      store.let_go_landed(&pinned, Some("homes")),
      Err(LetGoRefused::Pinned { by: "homes" })
    );
    let (freed, kept) =
      store.let_go_object_landed(&object, &|id| (*id == pinned).then_some("homes"));
    assert_eq!(kept, 1, "the pinned window kept");
    assert_eq!(freed, u64::try_from(bytes.len() - window).unwrap());
    assert!(!store.is_away(&pinned));
    assert!(
      all_away_but(&store, &windows, &pinned),
      "every other window freed"
    );
    let (freed, kept) = store.let_go_object_landed(&object, &|_| None);
    assert_eq!((freed, kept), (u64::try_from(window).unwrap(), 0));
    assert_eq!(store.bring_back_object(&object, &bytes), Ok(4));
    assert_eq!(
      store.object_bytes(&object, u64::try_from(bytes.len()).unwrap()),
      Some(bytes)
    );
  }

  /// The windows of `bytes`, put in `store` a window of `window` bytes each, each held twice more and marked landed (a
  /// landed push's holders: its staged pack, its repository's), each refused a plain `let_go` as shared.
  fn landed_with_two_more_holders(
    store: &mut ContentStore,
    bytes: &[u8],
    window: usize,
  ) -> Vec<ContentId> {
    let windows: Vec<ContentId> = bytes.chunks(window).map(ContentId::of).collect();
    for id in &windows {
      store.hold(id).unwrap();
      store.hold(id).unwrap();
      assert!(store.mark_landed(id));
      assert_eq!(store.let_go(id), Err(LetGoRefused::Shared));
    }
    windows
  }

  /// Whether every one of `windows` but `kept` is let go from `store`.
  fn all_away_but(store: &ContentStore, windows: &[ContentId], kept: &ContentId) -> bool {
    windows
      .iter()
      .filter(|id| *id != kept)
      .all(|id| store.is_away(id))
  }

  /// A-767 C1 step 2b (main's ruling (a) of 2026-11-22 02:52: the image records the windows let go). Do: put an object
  /// of three windows and a part and a one-window object, let both go, carry the store's windows let go, lists and names
  /// into a fresh store as a restart's image does (`aways`, `lists`, `names`; nothing of their bytes), then hold each as
  /// its row would, bring each back by its bytes, and hold a window the image never named. Expect: every window carried
  /// as let go with its length and the changes naming each; the holds taken; each object brought back whole, the same
  /// bytes by identity; the window never named refused `NotHeld`, never adopted.
  #[test]
  fn windows_let_go_survive_a_restart_as_let_go_and_come_back_by_their_bytes() {
    let window = 4096usize;
    let mut before = ContentStore::new(1 << 22, window).unwrap();
    let large = patterned(window * 3 + 100);
    let small = vec![4u8; 700];
    let objects = [
      (before.put_object(&large).unwrap(), large.clone()),
      (before.put_object(&small).unwrap(), small.clone()),
    ];
    for (object, _) in &objects {
      before.let_go_object(object);
    }
    let aways: Vec<(ContentId, u64)> = before.aways().collect();
    assert_eq!(
      aways.len(),
      5,
      "four windows of the first and the second whole"
    );
    assert!(
      before.away_changes().iter().all(|(_, len)| len.is_some()),
      "each let go, named with its length"
    );
    let lists: Vec<(ContentId, Vec<u8>)> = before
      .lists()
      .map(|(id, bytes)| (id, bytes.to_vec()))
      .collect();
    let names: Vec<(ContentId, ContentId)> = before.names().collect();
    let mut after = ContentStore::new(1 << 22, window).unwrap();
    assert_eq!(
      after.restore_away(aways),
      0,
      "every window let go taken back"
    );
    assert_eq!(
      after.restore_nodes(lists, names),
      0,
      "every list and name taken back"
    );
    for (object, bytes) in &objects {
      after.hold(object).unwrap();
      after.bring_back_object(object, bytes).unwrap();
      let len = u64::try_from(bytes.len()).unwrap();
      assert_eq!(
        after.object_bytes(object, len).as_deref(),
        Some(bytes.as_slice())
      );
    }
    assert_eq!(
      after.hold(&ContentId::of(b"never named")),
      Err(ContentRefused::NotHeld)
    );
  }

  /// Goal 93 durability (main's order of 2026-11-24 08:55: Mercurial's own history pushed and restarted, the store not
  /// made again, `Unheld { 00manifest, rev: 4222 }`; here rev 4281 of its first 5,000). Do: hold the empty window twice,
  /// as a Mercurial store holds it for an empty file revision's bytes and for a manifest kept as an empty delta (a
  /// manifest whose text is its parent's); mark it landed and let it go both ways a landed text is let go
  /// (`let_go_landed` for a push's windows, `let_go` for an import's); carry what a restart's image carries of the
  /// windows let go into a fresh store. Expect: each refused `Empty`, nothing freed; the window held, never away; the
  /// image carrying nothing let go; the fresh store refusing nothing. Red before: let go of no bytes, carried with a
  /// length of 0, which `restore_away` refuses, so after a restart the manifest's row named a window the store did not
  /// hold and its replay was refused `Unheld`.
  #[test]
  fn the_empty_window_is_never_let_go_so_a_restart_never_loses_it() {
    let mut store = ContentStore::new(1 << 20, 4096).unwrap();
    let id = store.put(b"").unwrap();
    assert_eq!(store.put(b"").unwrap(), id, "one window, held twice");
    assert!(store.mark_landed(&id));
    assert_eq!(store.let_go_landed(&id, None), Err(LetGoRefused::Empty));
    store.hold(&id).unwrap();
    let mut alone = ContentStore::new(1 << 20, 4096).unwrap();
    let single = alone.put(b"").unwrap();
    alone.mark_landed(&single);
    assert_eq!(alone.let_go(&single), Err(LetGoRefused::Empty));
    assert_eq!(alone.let_go_object(&single), 0);
    for held in [&store, &alone] {
      assert_the_empty_window_is_held(held, &id);
    }
  }

  /// The empty window as a store that kept it shows it: held, never away, read as no bytes, and nothing let go for a
  /// restart's image to carry, so a fresh store refuses nothing.
  fn assert_the_empty_window_is_held(held: &ContentStore, id: &ContentId) {
    assert!(!held.is_away(id), "the empty window held");
    assert_eq!(held.len_of(id), Some(0));
    assert_eq!(held.bytes_of(id).as_deref(), Some(&b""[..]));
    let aways: Vec<(ContentId, u64)> = held.aways().collect();
    assert!(aways.is_empty(), "nothing let go to carry: {aways:?}");
    let mut after = ContentStore::new(1 << 20, 4096).unwrap();
    assert_eq!(
      after.restore_away(aways),
      0,
      "nothing refused at the restart"
    );
  }

  /// A-724 piece 4. Do: let go a landed window, then put the same bytes again (a new push bringing them). Expect: the put
  /// brings the window back (read whole, counted) and holds it once more.
  #[test]
  fn a_put_of_a_let_go_windows_bytes_brings_it_back() {
    let mut store = ContentStore::new(1 << 20, 4096).unwrap();
    let bytes = vec![3u8; 500];
    let id = store.put(&bytes).unwrap();
    store.mark_landed(&id);
    store.let_go(&id).unwrap();
    assert_eq!(store.put(&bytes).unwrap(), id);
    assert_eq!(store.bytes_of(&id).as_deref(), Some(bytes.as_slice()));
    assert_eq!(store.counters.brought_back, 1);
  }

  /// A-724 piece 4 ("never windows another repository's grant keeps"). Do: put a window twice (two holders), mark it
  /// landed, let it go; put a list and try to let it go. Expect: refused `Shared`, then `NotAWindow`, nothing freed.
  #[test]
  fn content_two_hold_or_no_window_is_never_let_go() {
    let mut store = ContentStore::new(1 << 20, 4096).unwrap();
    let bytes = vec![5u8; 300];
    let id = store.put(&bytes).unwrap();
    store.put(&bytes).unwrap();
    store.mark_landed(&id);
    assert_eq!(store.let_go(&id), Err(LetGoRefused::Shared));
    let big = vec![9u8; 4096 * 3];
    let object = store.put_object(&big).unwrap();
    let root = store.resolve(&object);
    assert!(!store.mark_landed(&root));
    assert_eq!(store.let_go(&root), Err(LetGoRefused::NotAWindow));
    assert_eq!(store.counters.let_go, 0);
  }
}
