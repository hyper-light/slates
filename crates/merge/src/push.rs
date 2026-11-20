//! A client's push as a work (A-618; §4.17.1 "A push is a work", items 2 to 4; shared piece 4).
//!
//! git and Mercurial push snapshots: a push names the tree its new head holds, and the version the client says it saw
//! (item 1) names the tree the work is based on. [`build`] turns the two into one work: the operations a work volume's
//! journal would hold had the client written the new tree over the old one, composed by the deriver
//! ([`crate::increment::compose_volume_planned`]) into the increment the green judges, its post-state the bytes the
//! client sent.
//!
//! **What changed is read from identities.** Two trees are compared entry by entry by the ids the tool named them by
//! (git's tree, blob and commit ids, which the store checked against their bytes on arrival, §4.17.1 objects item 5).
//! An entry whose id is the base's is unchanged, and a subtree whose id is the base's is never read, so a push costs
//! the directories on the paths it changed, never the tree (the walk `git diff-tree` makes). No byte is compared and no
//! edit position is guessed: a file whose id changed is replaced whole, which the deriver composes into one `Delete` of
//! the base length and one `Insert` of the new bytes (§4.16 "Declared operations"), so D-27's never-diff clause holds.
//! What the walk read is counted ([`Reads`]): the trees it read, the blobs whose bytes it took, and the subtrees it
//! skipped.
//!
//! **The operations.** A file the push adds is `Create` and its bytes; a file it changes, its bytes replaced whole; a
//! file it removes, `Unlink`. A directory it adds is `Mkdir` before what it holds; one it removes, `Rmdir` after what
//! it held is removed. An executable file is `SetMode` 0755, and a file that stops being executable `SetMode` 0644 (a
//! file a green records no mode for checks out 0644, the server's `DEFAULT_FILE_MODE`). A symbolic link is `Symlink` to
//! its target; a changed target is `Unlink` then `Symlink`, which the deriver composes into one `Symlink`. A submodule's
//! link (git's `160000`) is `Mknod` of a special node holding the commit's id, with no owner, time or permission bit,
//! since git records none (A-637); a link moved to another commit is `Unlink` then `Mknod`, and a removed one `Unlink`.
//! The deriver lays a node's encoding in the post-state after the files' and attributes' bytes, and the build lays it
//! there.
//!
//! **The head (item 3).** A push that moves the branch sets the green's head: one `SetXattr` of [`HEAD_ATTRIBUTE`] on
//! the green's root ([`ROOT`], the empty path, which the deriver and the engine hold always present) to the new head.
//! Two works that both move the branch meet on that attribute, and the verdict answers what the tools answer: the same
//! value accepts, and a different one is a `MetaMeta` conflict, another push having come first. The value is the
//! tool's own form of its head, which its server reads back (git: the commit id's raw bytes).
//!
//! **The client's bytes (item 4).** Once accepted, every path of the pushed tree holds exactly the bytes the client
//! sent, with its mode, its target or its commit, nothing else stands in the tree, and the root's head is the client's:
//! the client-fidelity test in this module's tests checks it after every push of generated histories.
//!
//! **What is refused, typed ([`PushRefused`]).** A base version the green does not hold; an object the push names that
//! the store cannot give as the kind the walk needs; a name slates cannot hold in a path (not UTF-8, empty, `.`, `..`,
//! or holding `/` or NUL), or one name twice in a tree; a symbolic link whose target slates cannot hold; a path that
//! changes between a file, a directory, a symbolic link and a submodule's link, whose composition the deriver owes
//! (GAPS §8f);
//! a work past the caller's room, which bounds a push of any size (an ops document names as many paths as its work
//! declares, A-621); and a base the green disagrees with, which the deriver refuses.
//!
//! **Bounds (§4.9).** The walk keeps an explicit stack, so a deep tree cannot exhaust the thread's. Every operation the
//! work keeps, every step the walk queues and every byte the post-state will hold is charged against the caller's room
//! as it is made, a file's length before its bytes are read, so a push past it is refused before it is held, and a
//! store whose trees nested without end is refused at the room.

use core::future::Future;
use core::ops::Bound;
use core::task::Poll;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::engine::{Green, Increment};
use crate::increment::{Base, DeriveError, PostStatePlan, VolumeOp};
use crate::ops_doc::{Op, OpKind, OpsDoc};
use crate::pace::{Pace, Slices, Ticks};
use crate::special::{SpecialKind, SpecialNode};

/// Format: the reserved attribute a green's root holds the branch's head in (§4.17.1 "A push is a work", item 3).
pub const HEAD_ATTRIBUTE: &str = "slates.vcs.head";

/// The path of a green's root: the empty path, as the server keys a volume's `/` (`merge_service::canonical_path`).
pub const ROOT: &str = "";

/// Format: the longest object id a tool names, in bytes: SHA-256's, which git's SHA-256 repositories use.
pub const LONGEST_ID: usize = 32;

/// Format: the permission bits git checks out an executable file with (its tree's `100755`).
const EXECUTABLE_MODE: u32 = 0o755;

/// Format: the permission bits git checks out any other file with (`100644`), which a file a green records no mode for
/// checks out with too (the server's `DEFAULT_FILE_MODE`).
const FILE_MODE: u32 = 0o644;
/// Format: the permission bits of a submodule's link: none, as git records it (`160000`; A-637).
const GITLINK_MODE: u32 = 0;

/// Format: the BLAKE3 key-derivation context a push's work identity is hashed under, so it is never a work volume's.
const IDENTITY_CONTEXT: &str = "slates 2026-11-19 a client's push as a work";

/// The tool's name for an object: the raw bytes of its id, at most [`LONGEST_ID`] of them (git's 20 or 32 bytes,
/// Mercurial's 20).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectKey {
  /// The id's length.
  len: u8,
  /// The id's bytes, zeros past its length.
  bytes: [u8; LONGEST_ID],
}

impl ObjectKey {
  /// The key of the id `bytes`; none when they are longer than [`LONGEST_ID`].
  pub fn new(bytes: &[u8]) -> Option<ObjectKey> {
    let len = u8::try_from(bytes.len())
      .ok()
      .filter(|_| bytes.len() <= LONGEST_ID)?;
    let mut held = [0; LONGEST_ID];
    held.get_mut(..bytes.len())?.copy_from_slice(bytes);
    Some(ObjectKey { len, bytes: held })
  }

  /// The id's bytes.
  pub fn as_bytes(&self) -> &[u8] {
    self
      .bytes
      .get(..usize::from(self.len))
      .unwrap_or(&self.bytes)
  }
}

impl core::fmt::Debug for ObjectKey {
  /// The id in lower-case hexadecimal.
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    for byte in self.as_bytes() {
      write!(f, "{byte:02x}")?;
    }
    Ok(())
  }
}

/// What an entry of a tree is, as the tool's checkout reads its mode (git's `canon_mode`: a regular file is executable
/// or not, whatever other permission bits it was written with).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
  /// A regular file, not executable (git's `100644`).
  File,
  /// An executable regular file (`100755`).
  Executable,
  /// A symbolic link, whose blob holds its target (`120000`).
  Symlink,
  /// A subtree (`40000`).
  Directory,
  /// A link to another repository's commit (git's submodule, `160000`).
  Submodule,
}

/// One entry of a tree: a name of one path component, what it is, and the object it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
  /// The name, as the tool holds it.
  pub name: Vec<u8>,
  /// What it is.
  pub kind: EntryKind,
  /// The object it names.
  pub id: ObjectKey,
}

/// Why an object the walk needs cannot be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unavailable {
  /// The store holds no object of that id.
  Missing,
  /// The object is not a tree.
  NotATree,
  /// The object is not a blob.
  NotABlob,
  /// The blob's bytes are not the length the store said they were.
  LengthDiffers {
    /// The length the store said.
    said: u64,
    /// The length of the bytes it gave.
    given: u64,
  },
  /// The object's bytes are not an object of its type as its tool reads one: a tree git's tree walk cannot read, or
  /// a type the tool has no name for.
  Malformed,
  /// Listing the tree needs more room than the store has left.
  PastRoom {
    /// The bytes the listing asked for.
    asked: u64,
    /// The bytes the store had left.
    room: u64,
  },
}

/// The objects a push names, read by their ids: the repository's store with the push's own objects above it in its
/// quarantine (§4.17.1 objects items 3 and 4). A tool's store implements it (git's over `slates-vcs`, §4.17.2 piece 5);
/// every object it gives is one it checked against its id on arrival (item 5).
pub trait PushObjects {
  /// The entries of the tree `id`, in any order.
  fn tree(&self, id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable>;
  /// The entries of the tree `id` to read an entry a unit ([`Listing`], A-752 part 2b): a store that can decode its
  /// trees an entry at a time gives the tree's bytes and its decoder, so a build's unit is one entry, never a tree;
  /// else the whole listing, read in one unit.
  fn listing(&self, id: ObjectKey) -> Result<Listing, Unavailable> {
    self.tree(id).map(Listing::of)
  }
  /// The length in bytes of the blob `id`, which the store's index row holds, read before its bytes are.
  fn blob_len(&self, id: ObjectKey) -> Result<u64, Unavailable>;
  /// The bytes of the blob `id`.
  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable>;
}

/// A tree's entries read an entry a unit ([`PushObjects::listing`]; A-752 part 2b): listed whole, or the tree's bytes
/// owned and decoded an entry at each read, so a build over a directory of any width reads it over as many turns.
pub struct Listing {
  /// Where the entries come from.
  source: ListingSource,
}

/// A tree's own decoder: the entry beginning at an offset of its bytes and where the next begins, none past the last.
pub type Decoder =
  Box<dyn FnMut(&[u8], usize) -> Result<Option<(TreeEntry, usize)>, Unavailable> + Send>;

/// Where a [`Listing`]'s entries come from.
enum ListingSource {
  /// Listed whole.
  Listed(std::vec::IntoIter<TreeEntry>),
  /// The tree's bytes, the offset of the next entry, and its decoder; none once read to the end or refused.
  Read(Vec<u8>, Option<usize>, Decoder),
}

impl Listing {
  /// The entries listed whole.
  pub fn of(entries: Vec<TreeEntry>) -> Listing {
    Listing {
      source: ListingSource::Listed(entries.into_iter()),
    }
  }

  /// The tree `bytes`, decoded an entry at each read by `decode`.
  pub fn read(bytes: Vec<u8>, decode: Decoder) -> Listing {
    Listing {
      source: ListingSource::Read(bytes, Some(0), decode),
    }
  }

  /// The next entry; none past the last or after a refusal.
  pub fn next_entry(&mut self) -> Option<Result<TreeEntry, Unavailable>> {
    match &mut self.source {
      ListingSource::Listed(entries) => entries.next().map(Ok),
      ListingSource::Read(bytes, at, decode) => {
        let from = (*at)?;
        match decode(bytes, from) {
          Ok(Some((entry, next))) => {
            *at = Some(next);
            Some(Ok(entry))
          }
          Ok(None) => {
            *at = None;
            None
          }
          Err(why) => {
            *at = None;
            Some(Err(why))
          }
        }
      }
    }
  }
}

/// A push to one branch, as its work is built.
#[derive(Clone, Copy, Debug)]
pub struct Push<'a> {
  /// The caller's name for this push: two submits of one push carry the same, and the work's identity begins with it.
  pub name: [u8; 32],
  /// The version the client says it saw: the work's base (item 1).
  pub base_version: u64,
  /// That version's tree; none for a branch with nothing in it yet.
  pub base_tree: Option<ObjectKey>,
  /// The tree the pushed head holds.
  pub tree: ObjectKey,
  /// The new head in the form the tool's server reads back from [`HEAD_ATTRIBUTE`] (git: the commit id's raw bytes);
  /// none for a work that moves no head (Subversion's, or one the server turns into the tool's objects, item 3).
  pub head: Option<&'a [u8]>,
}

/// What a build read, so a test can prove it read only what changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reads {
  /// The trees it read: both of each directory whose id changed, and each directory only one side holds.
  pub trees: u64,
  /// The blobs whose bytes it took: each added or changed file that has bytes, and each added or changed symbolic link.
  pub blobs: u64,
  /// The subtrees it never read, the base and the push naming the same tree at their path.
  pub skipped: u64,
}

/// A push refused, typed, before any work is made of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushRefused {
  /// The base version is past the green's head: no version the green holds.
  UnknownBase {
    /// The push's base version.
    base: u64,
    /// The green's head.
    head: u64,
  },
  /// An object the push names, at `path`, that the store cannot give as the kind the walk needs.
  Object {
    /// The path that names it (the empty path for the root tree).
    path: String,
    /// The object's id.
    id: ObjectKey,
    /// Why.
    why: Unavailable,
  },
  /// A name slates cannot hold in a path: not UTF-8, empty, `.` or `..`, or holding `/` or NUL.
  NameNotHeld {
    /// The path's bytes, the name last.
    path: Vec<u8>,
  },
  /// A tree that holds one name twice.
  DuplicateName {
    /// The name's path.
    path: String,
  },
  /// A symbolic link whose target slates cannot hold: not UTF-8, or holding NUL.
  TargetNotHeld {
    /// The link's path.
    path: String,
  },
  /// A path that is a file, a directory, a symbolic link or a submodule's link on one side and another on the other,
  /// whose composition the deriver owes (GAPS §8f).
  KindChanged {
    /// The path.
    path: String,
  },
  /// What the work would keep is past the caller's room.
  PastRoom {
    /// The bytes the work would keep.
    asked: u64,
    /// The room it was given.
    room: u64,
  },
  /// The deriver refused the work against the green's base: a base the green disagrees with.
  Compose(DeriveError),
  /// The document names bytes at a path the walk gave none: never met, and refused rather than written as zeros.
  Unplaced {
    /// The path.
    path: String,
  },
}

/// A push's work, and what building it read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
  /// The work, to submit to the green.
  pub increment: Increment,
  /// What building it read.
  pub reads: Reads,
}

/// Builds `push`'s work against `green`, reading its objects from `objects`, within `room` bytes (module docs): the
/// increment to submit, and what the walk read; or the refusal. [`Building`] run to its end in one call.
pub fn build<O: PushObjects>(
  push: &Push<'_>,
  green: &Green,
  objects: &O,
  room: u64,
) -> Result<Built, PushRefused> {
  let mut building = Building::new(push, room, Pace::unbounded());
  loop {
    if let Some(built) = building.advance(green, objects) {
      return built;
    }
  }
}

/// The identity of a push's work: the push's name and base, then the document and the post-state, under a context of
/// its own (§4.16 data model, as `compose_increment` names a work volume's). What [`Building`] hashes a piece a unit:
/// the tests' oracle of it.
#[cfg(test)]
pub(crate) fn identity(name: &[u8; 32], base: u64, doc: &OpsDoc, post_state: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_CONTEXT);
  hasher.update(name);
  hasher.update(&base.to_le_bytes());
  hasher.update(&doc.encode());
  hasher.update(post_state);
  *hasher.finalize().as_bytes()
}

/// A push's work built a unit at a time (A-752; §4.3's step budget; the module's [`build`] in turns): the walk an entry
/// of a tree a unit, the base a named path a unit, the deriver's composition under the same pace
/// ([`crate::increment::compose_paced`]), the post-state an operation or a copy slice a unit, and the identity a piece
/// of the document or a hash slice a unit (the owner's derived [`Slices`]). It owns what it has made, and borrows the green and the objects only for a turn
/// ([`Building::advance`]), so a push's decision spans as many turns as its work needs and no turn runs past its pace.
///
/// The green may move between two turns: each named path's base is read at the push's base version, which no later
/// version changes ([`Green::base_entries`]), and the append commits the work only at the position its verdict read.
///
/// Units that are still one object's size, owed and named: a tree's listing ([`PushObjects::tree`] gives a whole tree)
/// and a blob's bytes ([`PushObjects::blob`] gives a whole blob, laid in one unit), the green's hard links (none in a
/// git or Mercurial green) and a copy's tree (a push makes none).
pub struct Building {
  /// The push.
  push: OwnedPush,
  /// The room.
  room: u64,
  /// The pace each turn runs under.
  pace: Pace,
  /// Where the build is.
  stage: Stage,
}

/// A [`Push`] owned across turns.
struct OwnedPush {
  name: [u8; 32],
  base_version: u64,
  base_tree: Option<ObjectKey>,
  tree: ObjectKey,
  head: Option<Vec<u8>>,
}

/// The deriver's composition in progress, owning its base and journal.
type Composing =
  core::pin::Pin<Box<dyn Future<Output = Result<(OpsDoc, PostStatePlan), DeriveError>> + Send>>;

/// Where a [`Building`] is.
enum Stage {
  /// Not begun.
  Begin,
  /// Walking the trees.
  Walk(Walk),
  /// Naming the paths the journal names, an operation a unit.
  Named {
    walk: Walk,
    at: usize,
    named: BTreeSet<String>,
  },
  /// Reading each named path's base entries, a path a unit.
  Entries {
    walk: Walk,
    named: BTreeSet<String>,
    after: Option<String>,
    base: Base,
  },
  /// Bringing each copy's tree, a copy a unit.
  Copies { walk: Walk, at: usize, base: Base },
  /// Composing.
  Compose { walk: Walk, work: Composing },
  /// Laying the post-state.
  Post(Box<Posting>),
  /// Hashing the identity.
  Identity(Box<Hashing>),
  /// Answered.
  Done,
}

impl Building {
  /// A build of `push` within `room` bytes, each turn under `pace`.
  pub fn new(push: &Push<'_>, room: u64, pace: Pace) -> Building {
    Building {
      push: OwnedPush {
        name: push.name,
        base_version: push.base_version,
        base_tree: push.base_tree,
        tree: push.tree,
        head: push.head.map(<[u8]>::to_vec),
      },
      room,
      pace,
      stage: Stage::Begin,
    }
  }

  /// The pace its turns run under: its owner begins each turn on it ([`Pace::turn`]).
  pub fn pace(&self) -> &Pace {
    &self.pace
  }

  /// Whether it is `push`'s build: the same name, base, trees and head.
  pub fn is_of(&self, push: &Push<'_>) -> bool {
    let own = &self.push;
    own.name == push.name
      && own.base_version == push.base_version
      && own.base_tree == push.base_tree
      && own.tree == push.tree
      && own.head.as_deref() == push.head
  }

  /// One turn over `green` and `objects`: units while the pace grants them. The work or the refusal once the build is
  /// done; none while units remain, for the next turn.
  pub fn advance<O: PushObjects>(
    &mut self,
    green: &Green,
    objects: &O,
  ) -> Option<Result<Built, PushRefused>> {
    loop {
      // The composition asks the pace itself, a unit at each of its own ticks; every other stage asks before a unit.
      // So a turn whose budget is shorter than one unit still runs one unit of the composition (its first tick takes
      // the turn's first unit), and the build always moves.
      let composing = matches!(self.stage, Stage::Compose { .. });
      if !composing && !self.pace.more() {
        return None;
      }
      match self.unit(green, objects) {
        Ok(None) => {}
        Ok(Some(built)) => {
          self.stage = Stage::Done;
          return Some(Ok(built));
        }
        Err(refused) => {
          self.stage = Stage::Done;
          return Some(Err(refused));
        }
      }
      // A composition still pending spent the turn: it asked for a unit past the deadline and was refused. Asking
      // again would time the drive between the two asks as no unit's.
      if composing && matches!(self.stage, Stage::Compose { .. }) {
        return None;
      }
    }
  }

  /// One unit: the work once done.
  fn unit<O: PushObjects>(
    &mut self,
    green: &Green,
    objects: &O,
  ) -> Result<Option<Built>, PushRefused> {
    let stage = core::mem::replace(&mut self.stage, Stage::Done);
    self.stage = match stage {
      Stage::Begin => self.begin(green)?,
      Stage::Walk(walk) => self.walked(walk, objects)?,
      Stage::Named { walk, at, named } => named_unit(walk, at, named),
      Stage::Entries {
        walk,
        named,
        after,
        base,
      } => self.entries_unit(green, walk, (named, after), base),
      Stage::Copies { walk, at, base } => self.copies_unit(green, walk, at, base),
      Stage::Compose { walk, work } => self.compose_unit(walk, work)?,
      Stage::Post(mut posting) => {
        match posting.unit(objects, self.push.head.as_deref(), self.pace.slices())? {
          None => Stage::Post(posting),
          Some((doc, post_state, reads)) => Stage::Identity(Box::new(Hashing::new(
            (&self.push.name, self.push.base_version),
            doc,
            post_state,
            reads,
          ))),
        }
      }
      Stage::Identity(mut hashing) => {
        if hashing.unit(self.pace.slices()) {
          return Ok(Some(hashing.built(self.push.base_version)));
        }
        Stage::Identity(hashing)
      }
      Stage::Done => Stage::Done,
    };
    Ok(None)
  }

  /// A unit of the base: the next named path's entries; past the last, the hard links, and the copies begun.
  fn entries_unit(
    &self,
    green: &Green,
    walk: Walk,
    (named, after): (BTreeSet<String>, Option<String>),
    mut base: Base,
  ) -> Stage {
    let next = match &after {
      None => named.iter().next(),
      Some(after) => named
        .range::<str, _>((Bound::Excluded(after.as_str()), Bound::Unbounded))
        .next(),
    };
    let Some(path) = next else {
      base.hardlinks = green.base_links(self.push.base_version, |path| named.contains(path));
      return Stage::Copies { walk, at: 0, base };
    };
    green.base_entries(self.push.base_version, path, &mut base);
    let after = Some(path.clone());
    Stage::Entries {
      walk,
      named,
      after,
      base,
    }
  }

  /// A unit of the copies: the next operation's, where it is a copy; past the last, the composition begun.
  fn copies_unit(&self, green: &Green, walk: Walk, at: usize, mut base: Base) -> Stage {
    let Some(op) = walk.journal.get(at) else {
      return self.composing(walk, base);
    };
    if let VolumeOp::Copy {
      from, to, version, ..
    } = op
    {
      green.base_copied((from, to, *version), &mut base);
    }
    Stage::Copies {
      walk,
      at: at.saturating_add(1),
      base,
    }
  }

  /// A turn of the composition, driven under the build's pace; once composed, the post-state begun.
  fn compose_unit(&self, walk: Walk, mut work: Composing) -> Result<Stage, PushRefused> {
    match self.pace.drive(work.as_mut()) {
      Poll::Pending => Ok(Stage::Compose { walk, work }),
      Poll::Ready(composed) => {
        let (doc, plan) = composed.map_err(PushRefused::Compose)?;
        Ok(Stage::Post(Box::new(Posting::new(walk, doc, plan))))
      }
    }
  }

  /// The first unit: the base checked against the green, and the walk begun.
  fn begin(&self, green: &Green) -> Result<Stage, PushRefused> {
    if self.push.base_version > green.head() {
      return Err(PushRefused::UnknownBase {
        base: self.push.base_version,
        head: green.head(),
      });
    }
    let mut walk = Walk {
      journal: Vec::new(),
      contents: BTreeMap::new(),
      nodes: BTreeMap::new(),
      reads: Reads::default(),
      held: 0,
      room: self.room,
      stack: Vec::new(),
      comparing: None,
    };
    if self.push.base_tree == Some(self.push.tree) {
      walk.reads.skipped = 1;
    } else {
      walk.queue(Step::Compare {
        path: ROOT.to_owned(),
        base: self.push.base_tree,
        pushed: Some(self.push.tree),
      })?;
    }
    Ok(Stage::Walk(walk))
  }

  /// A unit of the walk; once it is done, the head's operation and the naming begun.
  fn walked<O: PushObjects>(&self, walk: Walk, objects: &O) -> Result<Stage, PushRefused> {
    let mut walker = Walker { objects, walk };
    if walker.unit()? {
      return Ok(Stage::Walk(walker.walk));
    }
    let mut walk = walker.walk;
    if let Some(head) = &self.push.head {
      walk.emit(VolumeOp::SetXattr {
        path: ROOT.to_owned(),
        name: HEAD_ATTRIBUTE.to_owned(),
        value: head.clone(),
      })?;
    }
    Ok(Stage::Named {
      walk,
      at: 0,
      named: BTreeSet::new(),
    })
  }

  /// The composition begun over the base and the journal, which it takes.
  fn composing(&self, mut walk: Walk, base: Base) -> Stage {
    let journal = core::mem::take(&mut walk.journal);
    let work: Composing = Box::pin(async move {
      let (base, journal) = (base, journal);
      crate::increment::compose_paced(&base, &journal, &Ticks).await
    });
    Stage::Compose { walk, work }
  }
}

/// A unit of the naming: the next operation's names; past the last, the base begun.
fn named_unit(walk: Walk, at: usize, mut named: BTreeSet<String>) -> Stage {
  let Some(op) = walk.journal.get(at) else {
    return Stage::Entries {
      walk,
      named,
      after: None,
      base: Base::of_files(Vec::new()),
    };
  };
  for name in op.names().into_iter().flatten() {
    if !named.contains(name) {
      named.insert(name.to_owned());
    }
  }
  Stage::Named {
    walk,
    at: at.saturating_add(1),
    named,
  }
}

/// One step of the walk.
enum Step {
  /// Compare the trees at `path`: the base's (none where the base has no directory) and the push's (none where the push
  /// removes it).
  Compare {
    /// The directory's path.
    path: String,
    /// The base's tree there.
    base: Option<ObjectKey>,
    /// The push's tree there.
    pushed: Option<ObjectKey>,
  },
  /// Remove the directory at the path, once everything beneath it is removed.
  Rmdir(String),
}

impl Step {
  /// The bytes the step holds while it waits on the stack.
  fn footprint(&self) -> u64 {
    let path = match self {
      Step::Compare { path, .. } | Step::Rmdir(path) => path.len(),
    };
    to_u64(size_of::<Step>().saturating_add(path))
  }
}

/// The trees at one path being compared, an entry a unit: the base's listing named, then the push's, then each name.
struct Comparing {
  /// The directory's path.
  path: String,
  /// The base's tree there.
  base: Option<ObjectKey>,
  /// The push's tree there.
  pushed: Option<ObjectKey>,
  /// Where the comparison is.
  side: Side,
  /// The listing being named, and its tree.
  listed: (Listing, Option<ObjectKey>),
  /// The base's entries by name.
  based: BTreeMap<String, (EntryKind, ObjectKey)>,
  /// The push's entries by name.
  given: BTreeMap<String, (EntryKind, ObjectKey)>,
}

/// Where a [`Comparing`] is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
  /// The base's tree to read.
  ReadBase,
  /// The base's listing being named.
  NameBase,
  /// The push's tree to read.
  ReadPushed,
  /// The push's listing being named.
  NamePushed,
  /// Each name compared.
  Entries,
}

/// A walk in progress: what it has made and read, and what is left, owned across turns.
struct Walk {
  /// The operations made so far.
  journal: Vec<VolumeOp>,
  /// Each added or changed file's blob and its length, by path: the bytes the post-state takes.
  contents: BTreeMap<String, (ObjectKey, u64)>,
  /// Each submodule link made, by path: its node, whose encoding the post-state takes (A-637).
  nodes: BTreeMap<String, SpecialNode>,
  /// What the walk read.
  reads: Reads,
  /// The bytes charged.
  held: u64,
  /// The room.
  room: u64,
  /// The steps left, the next on top.
  stack: Vec<Step>,
  /// The trees being compared, before the next step.
  comparing: Option<Comparing>,
}

impl Walk {
  /// Charges `bytes` against the room.
  fn charge(&mut self, bytes: u64) -> Result<(), PushRefused> {
    self.held = self.held.saturating_add(bytes);
    if self.held > self.room {
      return Err(PushRefused::PastRoom {
        asked: self.held,
        room: self.room,
      });
    }
    Ok(())
  }

  /// Keeps `op`, charged first.
  fn emit(&mut self, op: VolumeOp) -> Result<(), PushRefused> {
    self.charge(op.footprint())?;
    self.journal.push(op);
    Ok(())
  }

  /// Queues `step`, charged first.
  fn queue(&mut self, step: Step) -> Result<(), PushRefused> {
    self.charge(step.footprint())?;
    self.stack.push(step);
    Ok(())
  }
}

/// A walk lent the objects for a turn.
struct Walker<'o, O> {
  /// The objects.
  objects: &'o O,
  /// The walk.
  walk: Walk,
}

impl<O> core::ops::Deref for Walker<'_, O> {
  type Target = Walk;

  fn deref(&self) -> &Walk {
    &self.walk
  }
}

impl<O> core::ops::DerefMut for Walker<'_, O> {
  fn deref_mut(&mut self) -> &mut Walk {
    &mut self.walk
  }
}

impl<O: PushObjects> Walker<'_, O> {
  /// One unit of the walk: an entry of the trees being compared, or the next step. Whether units remain.
  fn unit(&mut self) -> Result<bool, PushRefused> {
    if let Some(mut comparing) = self.walk.comparing.take() {
      if self.compare_unit(&mut comparing)? {
        self.walk.comparing = Some(comparing);
      }
      return Ok(true);
    }
    match self.walk.stack.pop() {
      Some(Step::Rmdir(path)) => self.emit(VolumeOp::Rmdir { path })?,
      Some(Step::Compare { path, base, pushed }) => {
        self.walk.comparing = Some(Comparing {
          path,
          base,
          pushed,
          side: Side::ReadBase,
          listed: (Listing::of(Vec::new()), None),
          based: BTreeMap::new(),
          given: BTreeMap::new(),
        });
      }
      None => return Ok(false),
    }
    Ok(true)
  }

  /// One unit of a comparison, entry by entry by name: whether units of it remain.
  fn compare_unit(&mut self, comparing: &mut Comparing) -> Result<bool, PushRefused> {
    match comparing.side {
      Side::ReadBase => {
        comparing.listed = (
          self.listing(&comparing.path, comparing.base)?,
          comparing.base,
        );
        comparing.side = Side::NameBase;
      }
      Side::ReadPushed => {
        comparing.listed = (
          self.listing(&comparing.path, comparing.pushed)?,
          comparing.pushed,
        );
        comparing.side = Side::NamePushed;
      }
      Side::NameBase | Side::NamePushed => {
        let base_side = comparing.side == Side::NameBase;
        let (listing, tree) = &mut comparing.listed;
        match listing.next_entry() {
          Some(Err(why)) => {
            // Only a tree read refuses, so the listing has its tree.
            let path = comparing.path.clone();
            let Some(id) = *tree else {
              return Err(PushRefused::Unplaced { path });
            };
            return Err(PushRefused::Object { path, id, why });
          }
          Some(Ok(TreeEntry {
            name,
            kind,
            id: named,
          })) => {
            let name = held_name(&comparing.path, name)?;
            let entries = if base_side {
              &mut comparing.based
            } else {
              &mut comparing.given
            };
            if entries.contains_key(&name) {
              return Err(PushRefused::DuplicateName {
                path: joined(&comparing.path, &name),
              });
            }
            entries.insert(name, (kind, named));
          }
          None => {
            comparing.side = if base_side {
              Side::ReadPushed
            } else {
              Side::Entries
            };
          }
        }
      }
      Side::Entries => {
        if let Some((name, was)) = comparing.based.pop_first() {
          let now = comparing.given.remove(&name);
          self.entry(joined(&comparing.path, &name), Some(was), now)?;
        } else if let Some((name, now)) = comparing.given.pop_first() {
          self.entry(joined(&comparing.path, &name), None, Some(now))?;
        } else {
          return Ok(false);
        }
      }
    }
    Ok(true)
  }

  /// The listing of the tree `id` at `path`, to name an entry a unit ([`PushObjects::listing`]); none for no tree.
  fn listing(&mut self, path: &str, id: Option<ObjectKey>) -> Result<Listing, PushRefused> {
    let Some(id) = id else {
      return Ok(Listing::of(Vec::new()));
    };
    self.reads.trees = self.reads.trees.saturating_add(1);
    self.objects.listing(id).map_err(|why| PushRefused::Object {
      path: path.to_owned(),
      id,
      why,
    })
  }

  /// The operations for one path: what the base held there and what the push holds.
  fn entry(
    &mut self,
    path: String,
    was: Option<(EntryKind, ObjectKey)>,
    now: Option<(EntryKind, ObjectKey)>,
  ) -> Result<(), PushRefused> {
    match (was, now) {
      (Some(was), Some(now)) => self.changed(path, was, now),
      (Some((kind, id)), None) => self.removed(path, kind, id),
      (None, Some((kind, id))) => self.added(path, kind, id),
      (None, None) => Ok(()),
    }
  }

  /// A path both sides hold.
  fn changed(
    &mut self,
    path: String,
    (was, was_id): (EntryKind, ObjectKey),
    (now, now_id): (EntryKind, ObjectKey),
  ) -> Result<(), PushRefused> {
    match (was, now) {
      (EntryKind::Directory, EntryKind::Directory) if was_id == now_id => {
        self.reads.skipped = self.reads.skipped.saturating_add(1);
        Ok(())
      }
      (EntryKind::Directory, EntryKind::Directory) => self.queue(Step::Compare {
        path,
        base: Some(was_id),
        pushed: Some(now_id),
      }),
      (EntryKind::Symlink, EntryKind::Symlink) if was_id == now_id => Ok(()),
      (EntryKind::Symlink, EntryKind::Symlink) => {
        let target = self.target(&path, now_id)?;
        self.emit(VolumeOp::Unlink { path: path.clone() })?;
        self.emit(VolumeOp::Symlink { path, target })
      }
      (EntryKind::File | EntryKind::Executable, EntryKind::File | EntryKind::Executable) => {
        if was_id != now_id {
          self.emit(VolumeOp::Truncate {
            path: path.clone(),
            len: 0,
          })?;
          self.write(&path, now_id)?;
        }
        if was != now {
          self.emit(VolumeOp::SetMode {
            path,
            mode: mode_of(now),
          })?;
        }
        Ok(())
      }
      (EntryKind::Submodule, EntryKind::Submodule) if was_id == now_id => Ok(()),
      (EntryKind::Submodule, EntryKind::Submodule) => {
        self.emit(VolumeOp::Unlink { path: path.clone() })?;
        self.link(path, now_id)
      }
      _ => Err(PushRefused::KindChanged { path }),
    }
  }

  /// A path only the base holds.
  fn removed(&mut self, path: String, kind: EntryKind, id: ObjectKey) -> Result<(), PushRefused> {
    match kind {
      EntryKind::Directory => {
        // Below its contents on the stack, so it is removed once they are.
        self.queue(Step::Rmdir(path.clone()))?;
        self.queue(Step::Compare {
          path,
          base: Some(id),
          pushed: None,
        })
      }
      EntryKind::File | EntryKind::Executable | EntryKind::Symlink | EntryKind::Submodule => {
        self.emit(VolumeOp::Unlink { path })
      }
    }
  }

  /// A path only the push holds.
  fn added(&mut self, path: String, kind: EntryKind, id: ObjectKey) -> Result<(), PushRefused> {
    match kind {
      EntryKind::Directory => {
        self.emit(VolumeOp::Mkdir { path: path.clone() })?;
        self.queue(Step::Compare {
          path,
          base: None,
          pushed: Some(id),
        })
      }
      EntryKind::File | EntryKind::Executable => {
        self.emit(VolumeOp::Create { path: path.clone() })?;
        self.write(&path, id)?;
        if kind == EntryKind::Executable {
          self.emit(VolumeOp::SetMode {
            path,
            mode: EXECUTABLE_MODE,
          })?;
        }
        Ok(())
      }
      EntryKind::Symlink => {
        let target = self.target(&path, id)?;
        self.emit(VolumeOp::Symlink { path, target })
      }
      EntryKind::Submodule => self.link(path, id),
    }
  }

  /// Makes `path` a submodule's link to `commit` (A-637; §4.17.2 "Trees as a volume" item 5): a special node holding the
  /// commit's id, with no owner or time, since git records none, its encoding charged now and laid at the seal.
  fn link(&mut self, path: String, commit: ObjectKey) -> Result<(), PushRefused> {
    let node = SpecialNode {
      kind: SpecialKind::Gitlink { commit },
      uid: 0,
      gid: 0,
      atime: 0,
      mtime: 0,
      ctime: 0,
      btime: 0,
    };
    let entry = size_of::<(String, SpecialNode)>()
      .saturating_add(path.len())
      .saturating_add(node.encoded_len());
    self.charge(to_u64(entry))?;
    self.nodes.insert(path.clone(), node);
    self.emit(VolumeOp::Mknod {
      path,
      node,
      mode: GITLINK_MODE,
    })
  }

  /// Gives the empty file at `path` the blob `id`'s bytes: its length charged now, its bytes taken at the seal.
  fn write(&mut self, path: &str, id: ObjectKey) -> Result<(), PushRefused> {
    let len = self
      .objects
      .blob_len(id)
      .map_err(|why| PushRefused::Object {
        path: path.to_owned(),
        id,
        why,
      })?;
    if len == 0 {
      return Ok(());
    }
    let entry = size_of::<(ObjectKey, u64)>().saturating_add(path.len());
    self.charge(len.saturating_add(to_u64(entry)))?;
    self.emit(VolumeOp::Extend {
      path: path.to_owned(),
      at: 0,
      len,
    })?;
    self.contents.insert(path.to_owned(), (id, len));
    Ok(())
  }

  /// The target the symbolic link `id` at `path` holds, its length charged before its bytes are read.
  fn target(&mut self, path: &str, id: ObjectKey) -> Result<String, PushRefused> {
    let refused = |why| PushRefused::Object {
      path: path.to_owned(),
      id,
      why,
    };
    let len = self.objects.blob_len(id).map_err(refused)?;
    self.charge(len)?;
    let bytes = self.objects.blob(id).map_err(refused)?;
    self.reads.blobs = self.reads.blobs.saturating_add(1);
    let given = to_u64(bytes.len());
    if given != len {
      return Err(refused(Unavailable::LengthDiffers { said: len, given }));
    }
    String::from_utf8(bytes.into_owned())
      .ok()
      .filter(|target| !target.contains('\0'))
      .ok_or_else(|| PushRefused::TargetNotHeld {
        path: path.to_owned(),
      })
  }
}

/// The post-state being laid, a unit an operation or a copy slice of zeros.
struct Posting {
  /// The document.
  doc: OpsDoc,
  /// Where each run comes from.
  plan: PostStatePlan,
  /// The walk's files, links and reads.
  walk: Walk,
  /// The post-state so far.
  post: Vec<u8>,
  /// Its length once laid; none until the operations have been read for it.
  size: Option<usize>,
  /// The end found so far.
  end: u64,
  /// The next operation.
  at: usize,
}

impl Posting {
  /// The post-state of `doc` to lay, as `plan` places its runs, from the walk's files.
  fn new(walk: Walk, doc: OpsDoc, plan: PostStatePlan) -> Posting {
    Posting {
      doc,
      plan,
      walk,
      post: Vec::new(),
      size: None,
      end: 0,
      at: 0,
    }
  }

  /// One unit: the length found an operation a unit, the post-state zeroed a copy slice a unit, then each operation's run
  /// laid. The document, the post-state and the reads once laid. Its length was charged as the walk made the runs.
  fn unit<O: PushObjects>(
    &mut self,
    objects: &O,
    head: Option<&[u8]>,
    slices: Slices,
  ) -> Result<Option<(OpsDoc, Vec<u8>, Reads)>, PushRefused> {
    let Some(size) = self.size else {
      match self.doc.ops.get(self.at) {
        Some(op) => {
          if op.src != u64::MAX && adds_bytes(op.kind) {
            self.end = self.end.max(op.src.saturating_add(op.len));
          }
          self.at = self.at.saturating_add(1);
        }
        None => {
          let size = usize::try_from(self.end).map_err(|_| PushRefused::PastRoom {
            asked: self.end,
            room: self.walk.room,
          })?;
          self.size = Some(size);
          self.post = Vec::new();
          self
            .post
            .try_reserve_exact(size)
            .map_err(|_| PushRefused::PastRoom {
              asked: self.end,
              room: self.walk.room,
            })?;
          self.at = 0;
        }
      }
      return Ok(None);
    };
    if self.post.len() < size {
      let unit = usize::try_from(slices.copy_bytes)
        .unwrap_or(usize::MAX)
        .max(1);
      let to = size.min(self.post.len().saturating_add(unit));
      self.post.resize(to, 0);
      return Ok(None);
    }
    let Some(op) = self.doc.ops.get(self.at).copied() else {
      let doc = core::mem::take(&mut self.doc);
      let post = core::mem::take(&mut self.post);
      return Ok(Some((doc, post, self.walk.reads)));
    };
    self.at = self.at.saturating_add(1);
    if op.src == u64::MAX {
      return Ok(None);
    }
    let Some(path) = self.doc.paths.path(op.path) else {
      return Ok(None);
    };
    let (source, from) = match op.kind {
      OpKind::Overwrite | OpKind::Insert | OpKind::Extend => {
        let bytes = file_bytes(&mut self.walk, objects, path)?;
        (bytes, self.plan.get(&op.src).copied())
      }
      OpKind::SetXattr => (Cow::Borrowed(head.unwrap_or_default()), Some(0)),
      OpKind::Mknod => {
        let node = self
          .walk
          .nodes
          .get(path)
          .ok_or_else(|| PushRefused::Unplaced {
            path: path.to_owned(),
          })?;
        (Cow::Owned(node.encode()), Some(0))
      }
      _ => return Ok(None),
    };
    lay(&mut self.post, &op, &source, from).ok_or_else(|| PushRefused::Unplaced {
      path: path.to_owned(),
    })?;
    Ok(None)
  }
}

/// The bytes of the blob the walk gave the file at `path`.
fn file_bytes<'o, O: PushObjects>(
  walk: &mut Walk,
  objects: &'o O,
  path: &str,
) -> Result<Cow<'o, [u8]>, PushRefused> {
  let (id, said) = walk
    .contents
    .get(path)
    .copied()
    .ok_or_else(|| PushRefused::Unplaced {
      path: path.to_owned(),
    })?;
  walk.reads.blobs = walk.reads.blobs.saturating_add(1);
  let refused = |why| PushRefused::Object {
    path: path.to_owned(),
    id,
    why,
  };
  let bytes = objects.blob(id).map_err(refused)?;
  let given = to_u64(bytes.len());
  if given != said {
    return Err(refused(Unavailable::LengthDiffers { said, given }));
  }
  Ok(bytes)
}

/// The identity being hashed, a piece of the document or a hash slice of the post-state a unit ([`identity`]'s bytes).
struct Hashing {
  /// The hasher.
  hasher: blake3::Hasher,
  /// The document.
  doc: OpsDoc,
  /// The post-state.
  post_state: Vec<u8>,
  /// What the walk read.
  reads: Reads,
  /// The next piece of the document.
  piece: usize,
  /// The post-state's bytes hashed.
  hashed: usize,
  /// A piece's bytes.
  scratch: Vec<u8>,
}

impl Hashing {
  /// The identity of `name`'s work on `base` to hash.
  fn new(
    (name, base): (&[u8; 32], u64),
    doc: OpsDoc,
    post_state: Vec<u8>,
    reads: Reads,
  ) -> Hashing {
    let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_CONTEXT);
    hasher.update(name);
    hasher.update(&base.to_le_bytes());
    Hashing {
      hasher,
      doc,
      post_state,
      reads,
      piece: 0,
      hashed: 0,
      scratch: Vec::new(),
    }
  }

  /// One unit: whether the identity is hashed whole.
  fn unit(&mut self, slices: Slices) -> bool {
    self.scratch.clear();
    if self.doc.encode_piece(self.piece, &mut self.scratch) {
      self.piece = self.piece.saturating_add(1);
      self.hasher.update(&self.scratch);
      return false;
    }
    let to = self.post_state.len().min(
      self.hashed.saturating_add(
        usize::try_from(slices.hash_bytes)
          .unwrap_or(usize::MAX)
          .max(1),
      ),
    );
    if let Some(bytes) = self.post_state.get(self.hashed..to) {
      self.hasher.update(bytes);
    }
    self.hashed = to;
    self.hashed == self.post_state.len()
  }

  /// The work, hashed.
  fn built(self, base: u64) -> Built {
    Built {
      increment: Increment {
        id: *self.hasher.finalize().as_bytes(),
        base,
        doc: self.doc,
        post_state: self.post_state,
        evidence: Vec::new(),
        named: Default::default(),
      },
      reads: self.reads,
    }
  }
}

/// Whether an operation of `kind` adds bytes the post-state holds.
fn adds_bytes(kind: OpKind) -> bool {
  matches!(
    kind,
    OpKind::Overwrite | OpKind::Insert | OpKind::Extend | OpKind::SetXattr | OpKind::Mknod
  )
}

/// Copies `op`'s run of `source`, from `from`, to the post-state at the op's `src`; none when either span is not whole.
fn lay(post: &mut [u8], op: &Op, source: &[u8], from: Option<u64>) -> Option<()> {
  let from = usize::try_from(from?).ok()?;
  let (at, len) = (usize::try_from(op.src).ok()?, usize::try_from(op.len).ok()?);
  let into = post.get_mut(at..at.checked_add(len)?)?;
  into.copy_from_slice(source.get(from..from.checked_add(len)?)?);
  Some(())
}

/// `name` checked as one component of a path under `prefix`: refused when slates cannot hold it.
fn held_name(prefix: &str, name: Vec<u8>) -> Result<String, PushRefused> {
  let refused = |name: &[u8]| {
    let mut path = prefix.as_bytes().to_vec();
    if !prefix.is_empty() {
      path.push(b'/');
    }
    path.extend_from_slice(name);
    PushRefused::NameNotHeld { path }
  };
  let unheld = name.is_empty()
    || name == b"."
    || name == b".."
    || name.contains(&b'/')
    || name.contains(&b'\0');
  if unheld {
    return Err(refused(&name));
  }
  String::from_utf8(name).map_err(|refusal| refused(refusal.as_bytes()))
}

/// `name` under `prefix`.
fn joined(prefix: &str, name: &str) -> String {
  if prefix.is_empty() {
    name.to_owned()
  } else {
    format!("{prefix}/{name}")
  }
}

/// The mode a file of `kind` checks out with.
fn mode_of(kind: EntryKind) -> u32 {
  if kind == EntryKind::Executable {
    EXECUTABLE_MODE
  } else {
    FILE_MODE
  }
}

/// A count or a length as `u64`, saturating where a `usize` would not fit.
fn to_u64(value: usize) -> u64 {
  u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
