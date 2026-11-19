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
//! An entry whose id is the base's is unchanged, and a subtree whose id is the base's is never read, so a push costs the
//! directories on the paths it changed, never the tree (the walk `git diff-tree` makes). No byte is compared and no edit
//! position is guessed: a file whose id changed is replaced whole, which the deriver composes into one `Delete` of the
//! base length and one `Insert` of the new bytes (§4.16 "Declared operations"), so D-27's never-diff clause holds. What
//! the walk read is counted ([`Reads`]): the trees it read, the blobs whose bytes it took, and the subtrees it skipped.
//!
//! **The operations.** A file the push adds is `Create` and its bytes; a file it changes, its bytes replaced whole; a
//! file it removes, `Unlink`. A directory it adds is `Mkdir` before what it holds; one it removes, `Rmdir` after what it
//! held is removed. An executable file is `SetMode` 0755, and a file that stops being executable `SetMode` 0644 (a file a
//! green records no mode for checks out 0644, the server's `DEFAULT_FILE_MODE`). A symbolic link is `Symlink` to its
//! target; a changed target is `Unlink` then `Symlink`, which the deriver composes into one `Symlink`.
//!
//! **The head (item 3).** A push that moves the branch sets the green's head: one `SetXattr` of [`HEAD_ATTRIBUTE`] on
//! the green's root ([`ROOT`], the empty path, which the deriver and the engine hold always present) to the new head.
//! Two works that both move the branch meet on that attribute, and the verdict answers what the tools answer: the same
//! value accepts, and a different one is a `MetaMeta` conflict, another push having come first. The value is the
//! tool's own form of its head, which its server reads back (git: the commit id's raw bytes).
//!
//! **The client's bytes (item 4).** Once accepted, every path of the pushed tree holds exactly the bytes the client
//! sent, with its mode or its target, nothing else stands in the tree, and the root's head is the client's: the
//! client-fidelity test in this module's tests checks it after every push of generated histories.
//!
//! **What is refused, typed ([`PushRefused`]).** A base version the green does not hold; an object the push names that
//! the store cannot give as the kind the walk needs; a name slates cannot hold in a path (not UTF-8, empty, `.`, `..`,
//! or holding `/` or NUL), or one name twice in a tree; a symbolic link whose target slates cannot hold; a link to
//! another repository's commit (a git submodule), whose special node kind is owed (§4.17.2 "Trees as a volume" item 5);
//! a path that changes between a file, a directory and a symbolic link, whose composition the deriver owes (GAPS §8f);
//! more names than one ops document holds; a work past the caller's room; and a base the green disagrees with, which
//! the deriver refuses.
//!
//! **Bounds (§4.9).** The walk keeps an explicit stack, so a deep tree cannot exhaust the thread's. Every operation the
//! work keeps, every step the walk queues and every byte the post-state will hold is charged against the caller's room
//! as it is made, a file's length before its bytes are read, so a push past it is refused before it is held, and a
//! store whose trees nested without end is refused at the room.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use crate::engine::{Green, Increment};
use crate::increment::{DeriveError, PostStatePlan, VolumeOp, compose_volume_planned};
use crate::ops_doc::{Op, OpKind, OpsDoc};

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

/// Format: the most names one ops document holds. Its path table's indices are `u16`, and its interning answers
/// `u16::MAX` for every name past them, so the last index is left unused (GAPS, A-618).
const MOST_NAMES: usize = u16::MAX as usize;

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
}

/// The objects a push names, read by their ids: the repository's store with the push's own objects above it in its
/// quarantine (§4.17.1 objects items 3 and 4). A tool's store implements it (git's over `slates-vcs`, §4.17.2 piece 5);
/// every object it gives is one it checked against its id on arrival (item 5).
pub trait PushObjects {
  /// The entries of the tree `id`, in any order.
  fn tree(&self, id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable>;
  /// The length in bytes of the blob `id`, which the store's index row holds, read before its bytes are.
  fn blob_len(&self, id: ObjectKey) -> Result<u64, Unavailable>;
  /// The bytes of the blob `id`.
  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable>;
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
  /// A link to another repository's commit (a git submodule), whose node kind is owed (§4.17.2 item 5).
  Submodule {
    /// The link's path.
    path: String,
  },
  /// A path that is a file, a directory or a symbolic link on one side and another on the other, whose composition the
  /// deriver owes (GAPS §8f).
  KindChanged {
    /// The path.
    path: String,
  },
  /// More names than one ops document holds.
  TooManyNames {
    /// The names the work would hold.
    names: usize,
    /// The most a document holds.
    most: usize,
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
/// increment to submit, and what the walk read; or the refusal.
pub fn build<O: PushObjects>(
  push: &Push<'_>,
  green: &Green,
  objects: &O,
  room: u64,
) -> Result<Built, PushRefused> {
  // Red: the walk is not built yet (A-618, failing tests first).
  let _ = (objects, room);
  Err(PushRefused::UnknownBase {
    base: push.base_version,
    head: green.head(),
  })
}

/// The identity of a push's work: the push's name and base, then the document and the post-state, under a context of
/// its own (§4.16 data model, as `compose_increment` names a work volume's).
pub(crate) fn identity(name: &[u8; 32], base: u64, doc: &OpsDoc, post_state: &[u8]) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(IDENTITY_CONTEXT);
  hasher.update(name);
  hasher.update(&base.to_le_bytes());
  hasher.update(&doc.encode());
  hasher.update(post_state);
  *hasher.finalize().as_bytes()
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

/// A walk in progress: what it has made and read, and what is left.
struct Walk<'o, O> {
  /// The objects.
  objects: &'o O,
  /// The operations made so far.
  journal: Vec<VolumeOp>,
  /// Each added or changed file's blob and its length, by path: the bytes the post-state takes.
  contents: BTreeMap<String, (ObjectKey, u64)>,
  /// What the walk read.
  reads: Reads,
  /// The bytes charged.
  held: u64,
  /// The room.
  room: u64,
  /// The steps left, the next on top.
  stack: Vec<Step>,
}

impl<O: PushObjects> Walk<'_, O> {
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

  /// Takes one step.
  fn step(&mut self, step: Step) -> Result<(), PushRefused> {
    match step {
      Step::Rmdir(path) => self.emit(VolumeOp::Rmdir { path }),
      Step::Compare { path, base, pushed } => self.compare(&path, base, pushed),
    }
  }

  /// Compares the trees at `path` entry by entry, by name.
  fn compare(
    &mut self,
    path: &str,
    base: Option<ObjectKey>,
    pushed: Option<ObjectKey>,
  ) -> Result<(), PushRefused> {
    let based = self.entries(path, base)?;
    let mut given = self.entries(path, pushed)?;
    for (name, was) in based {
      let now = given.remove(&name);
      self.entry(joined(path, &name), Some(was), now)?;
    }
    for (name, now) in given {
      self.entry(joined(path, &name), None, Some(now))?;
    }
    Ok(())
  }

  /// The entries of the tree `id` at `path` by name, each name checked; none for no tree.
  fn entries(
    &mut self,
    path: &str,
    id: Option<ObjectKey>,
  ) -> Result<BTreeMap<String, (EntryKind, ObjectKey)>, PushRefused> {
    let mut entries = BTreeMap::new();
    let Some(id) = id else {
      return Ok(entries);
    };
    self.reads.trees = self.reads.trees.saturating_add(1);
    let listed = self.objects.tree(id).map_err(|why| PushRefused::Object {
      path: path.to_owned(),
      id,
      why,
    })?;
    for TreeEntry {
      name,
      kind,
      id: named,
    } in listed
    {
      let name = held_name(path, name)?;
      if entries.contains_key(&name) {
        return Err(PushRefused::DuplicateName {
          path: joined(path, &name),
        });
      }
      entries.insert(name, (kind, named));
    }
    Ok(entries)
  }

  /// The operations for one path: what the base held there and what the push holds.
  fn entry(
    &mut self,
    path: String,
    was: Option<(EntryKind, ObjectKey)>,
    now: Option<(EntryKind, ObjectKey)>,
  ) -> Result<(), PushRefused> {
    match (was, now) {
      (Some((EntryKind::Submodule, _)), _) | (_, Some((EntryKind::Submodule, _))) => {
        Err(PushRefused::Submodule { path })
      }
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
      EntryKind::File | EntryKind::Executable | EntryKind::Symlink => {
        self.emit(VolumeOp::Unlink { path })
      }
      EntryKind::Submodule => Err(PushRefused::Submodule { path }),
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
      EntryKind::Submodule => Err(PushRefused::Submodule { path }),
    }
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

  /// Composes the journal against the green's base and lays its post-state out: the work.
  fn seal(mut self, push: &Push<'_>, green: &Green) -> Result<Built, PushRefused> {
    let names = names(&self.journal);
    if names > MOST_NAMES {
      return Err(PushRefused::TooManyNames {
        names,
        most: MOST_NAMES,
      });
    }
    let base = green.base_for(push.base_version, &self.journal);
    let (doc, plan) = compose_volume_planned(&base, &self.journal).map_err(PushRefused::Compose)?;
    let post_state = self.post_state(&doc, &plan, push.head.unwrap_or_default())?;
    let id = identity(&push.name, push.base_version, &doc, &post_state);
    Ok(Built {
      increment: Increment {
        id,
        base: push.base_version,
        doc,
        post_state,
        evidence: Vec::new(),
      },
      reads: self.reads,
    })
  }

  /// The post-state the document's runs name: each added or changed file's bytes, and the head's value, laid where the
  /// deriver's plan puts them (`compose_volume_planned`). Its length was charged as the walk made the runs.
  fn post_state(
    &mut self,
    doc: &OpsDoc,
    plan: &PostStatePlan,
    head: &[u8],
  ) -> Result<Vec<u8>, PushRefused> {
    let end = doc
      .ops
      .iter()
      .filter(|op| op.src != u64::MAX && adds_bytes(op.kind))
      .map(|op| op.src.saturating_add(op.len))
      .max()
      .unwrap_or(0);
    let size = usize::try_from(end).map_err(|_| PushRefused::PastRoom {
      asked: end,
      room: self.room,
    })?;
    let mut post = vec![0; size];
    for op in doc.ops.iter().filter(|op| op.src != u64::MAX) {
      let Some(path) = doc.paths.path(op.path) else {
        continue;
      };
      let (source, from) = match op.kind {
        OpKind::Overwrite | OpKind::Insert | OpKind::Extend => {
          (self.file_bytes(path)?, plan.get(&op.src).copied())
        }
        OpKind::SetXattr => (Cow::Borrowed(head), Some(0)),
        _ => continue,
      };
      lay(&mut post, op, &source, from).ok_or_else(|| PushRefused::Unplaced {
        path: path.to_owned(),
      })?;
    }
    Ok(post)
  }

  /// The bytes of the blob the walk gave the file at `path`.
  fn file_bytes(&mut self, path: &str) -> Result<Cow<'_, [u8]>, PushRefused> {
    let (id, said) = self
      .contents
      .get(path)
      .copied()
      .ok_or_else(|| PushRefused::Unplaced {
        path: path.to_owned(),
      })?;
    self.reads.blobs = self.reads.blobs.saturating_add(1);
    let refused = |why| PushRefused::Object {
      path: path.to_owned(),
      id,
      why,
    };
    let bytes = self.objects.blob(id).map_err(refused)?;
    let given = to_u64(bytes.len());
    if given != said {
      return Err(refused(Unavailable::LengthDiffers { said, given }));
    }
    Ok(bytes)
  }
}

/// Whether an operation of `kind` adds bytes the post-state holds.
fn adds_bytes(kind: OpKind) -> bool {
  matches!(
    kind,
    OpKind::Overwrite | OpKind::Insert | OpKind::Extend | OpKind::SetXattr
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

/// The names one document holding `journal` interns: every path an operation names, symbolic links' targets and
/// attributes' names.
fn names(journal: &[VolumeOp]) -> usize {
  let mut names: BTreeSet<&str> = BTreeSet::new();
  for op in journal {
    names.extend(op.names().into_iter().flatten());
    match op {
      VolumeOp::Symlink { target, .. } => {
        names.insert(target);
      }
      VolumeOp::SetXattr { name, .. } | VolumeOp::RemoveXattr { name, .. } => {
        names.insert(name);
      }
      _ => {}
    }
  }
  names.len()
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
