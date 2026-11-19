//! receive-pack's part 2 (A-620; §4.17.1 "A push is a work" items 1 to 5 and the namespace's items
//! 4 to 6; §4.17.2 "A push" items 6 and 7; A-642, A-645): each command's update made a work
//! through A-618's [`build`], read through the store's view of the push (A-638's `PushedObjects`),
//! and appended to its green's own log through [`Appends`].
//!
//! **The verdict reads, and moves nothing** ([`Works`], receive-pack's [`Judge`]). Each green is
//! read as the push found it ([`Greens`]), its state immutable at that position, so its work can be
//! computed anywhere (A-645: merges computed anywhere, committed by position):
//! - an update's base is the version whose head is the command's old value (§4.17.1 item 1); a
//!   value no version of the green held is answered as git answers a ref that moved, before any
//!   work is built;
//! - its work is A-618's: the whole-file operations between that version's tree and the tree the
//!   new value names, and the head operation (items 2 and 3);
//! - a create is made at the version of a green its new value's first parent heads, where one can
//!   still be branched from, else at the default branch's head ([`Greens::base_of_create`]), and its
//!   work states the whole difference against that version's tree, so the new green holds exactly
//!   the client's tree; in a repository whose default branch is unborn, over an empty green. A
//!   create of a ref that names a green is a ref that moved, as git's lock refuses a ref that exists
//!   (A-654, main's ruling of 2026-11-19);
//! - a delete retires the green, its old value the green's head, or the ref moved.
//!
//! A ref's tree is its commit's, or the tree it names; a tag (a green too, §4.17.1 item 6) may name
//! an annotated tag, whose tree is that of the commit or the tree it names. A tag of a tag, or a
//! ref naming a blob, is refused typed (owed: git peels any chain of tags).
//!
//! **The append moves the ref** ([`Appends`], A-642; g261's per-green append implements it once it
//! lands, with the store's commit of the push's objects, A-614's `Repository::commit`, beside it):
//! one entry on the green's own log, only while the green is still at the position the verdict
//! read; under `atomic`, every entry or none. A green that moved, or whose verdict at the append
//! meets a conflict on the head (another push came first), is git's `failed to update ref`, or
//! under `atomic` `atomic transaction failed` (§4.17.1 item 5: the words git gives a ref that
//! moved).
//!
//! **A work refused** (A-618's `PushRefused`) is answered in git's words where git has them (a ref
//! that moved, `missing necessary objects`) and in slates' own words otherwise, one reason for each
//! refusal, with a band-2 line naming the path.

use slates_merge::push::{ObjectKey, Push, PushObjects, PushRefused, build};
use slates_merge::{ConflictWindow, Green, Increment};

use super::{
  ATOMIC_TRANSACTION_FAILED, Command, Declined, FAILED_TO_UPDATE, Judge, MISSING_OBJECTS, Pushed,
  Reported, Repository, message,
};
use crate::Admission;
use crate::object_name::{Kind, ObjectId};
use crate::objects::commit::Commit;
use crate::objects::tag::Tag;

/// Slates' words: the reason a push naming a path slates cannot hold is given (A-618's
/// `NameNotHeld`: not UTF-8, empty, `.` or `..`, or holding `/` or NUL).
pub const NAME_NOT_HELD: &[u8] = b"path not held";

/// Slates' words: the reason a tree naming one name twice is given (A-618's `DuplicateName`).
pub const DUPLICATE_NAME: &[u8] = b"duplicate entries";

/// Slates' words: the reason a symbolic link whose target slates cannot hold is given (A-618's
/// `TargetNotHeld`).
pub const TARGET_NOT_HELD: &[u8] = b"link target not held";

/// Slates' words: the reason a path that changes between a file, a directory, a symbolic link and
/// a submodule's link is given while its composition is owed (A-618's `KindChanged`; GAPS §8f).
pub const KIND_CHANGED: &[u8] = b"path changes kind";

/// Slates' words: the reason a work past the push's room is given (A-618's `PastRoom`).
pub const PAST_ROOM: &[u8] = b"push past its room";

/// Slates' words: the reason a work naming bytes at a path the walk gave none is given (A-618's
/// `Unplaced`).
pub const UNPLACED: &[u8] = b"path without bytes";

/// Slates' words: the reason a ref naming neither a commit nor a tree, nor an annotated tag of
/// one, is given (a blob, or a tag of a tag).
pub const NO_TREE: &[u8] = b"ref names no tree";

/// A green as one push reads it.
#[derive(Clone, Copy, Debug)]
pub struct GreenRead<'g> {
  /// The green.
  pub id: u64,
  /// Where its own log stood when read: the position its append must still find (A-642).
  pub position: u64,
  /// Its state at that position, which nothing changes while the push is judged (A-645).
  pub green: &'g Green,
}

/// A version of a green: its number and the tree its head names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
  /// The version.
  pub number: u64,
  /// The tree its head names.
  pub tree: ObjectId,
}

/// The greens a repository's refs name, as one push reads them (§4.17.1: a branch or a tag is a
/// green).
pub trait Greens {
  /// The green the ref `name` names, read for this push; none for a ref that names none.
  fn read(&self, name: &[u8]) -> Option<GreenRead<'_>>;

  /// The version of `green` whose head is `head`, if one held it (§4.17.1 item 1: a push's base).
  fn version_of(&self, green: u64, head: &ObjectId) -> Option<Version>;

  /// The green and version a create whose new value's first parent is `parent` is made at (module
  /// docs): the version whose head is `parent` where one can still be branched from; else the
  /// default branch's head; none while the default branch is unborn.
  fn base_of_create(&self, parent: Option<&ObjectId>) -> Option<(GreenRead<'_>, Version)>;
}

/// What a command's verdict carries to its append: one entry for a green's own log (A-642).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
  /// A work on a green, appended only while the green is at `position`.
  Work {
    /// The green.
    green: u64,
    /// The position its verdict read.
    position: u64,
    /// The work.
    increment: Increment,
  },
  /// A new green the ref `name` names, made only while the name names none.
  Create {
    /// The ref.
    name: Vec<u8>,
    /// The green and version it is made at ([`Greens::base_of_create`]); none to make it empty.
    from: Option<(u64, u64)>,
    /// Its first work, built against that version, or over an empty green.
    increment: Increment,
  },
  /// The green retired (§4.17.1: a deleted branch's chain kept for its retention), only while it
  /// is at `position`.
  Retire {
    /// The green.
    green: u64,
    /// The position its verdict read.
    position: u64,
  },
}

/// Why an entry was not appended, and which: its place among the entries given (0 for one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unappended {
  /// Its green moved since its position was read, or a create's name came to name a green.
  Moved {
    /// The entry's place.
    at: usize,
  },
  /// The verdict at the append met a conflict: on the head, another push came first.
  Conflict {
    /// The entry's place.
    at: usize,
    /// The conflict's windows.
    windows: Vec<ConflictWindow>,
  },
}

/// The appends that move refs (A-642: each green its own log; module docs).
pub trait Appends {
  /// Appends one entry on its green's own log.
  fn append(&mut self, entry: Entry) -> Result<(), Unappended>;

  /// Appends an atomic push's entries, each on its green's own log: every one or none.
  fn append_atomic(&mut self, entries: Vec<Entry>) -> Result<(), Unappended>;
}

/// receive-pack's part 2 judge (module docs).
pub struct Works<'w, G: ?Sized, R: ?Sized, O, A: ?Sized> {
  /// The greens as the push reads them.
  greens: &'w G,
  /// The repository's indexed objects, for a commit or tag the push did not bring.
  repository: &'w R,
  /// The push's trees and blobs, the index first and then the push's quarantine.
  objects: &'w O,
  /// Where the works are appended.
  appends: &'w mut A,
  /// The push's name, which every work's identity begins with (A-618).
  name: [u8; 32],
  /// The room each work is built within.
  room: u64,
}

impl<'w, G, R, O, A> Works<'w, G, R, O, A>
where
  G: Greens + ?Sized,
  R: Repository + ?Sized,
  O: PushObjects,
  A: Appends + ?Sized,
{
  /// A judge of the push named `name` (two submits of one push carry one name): its greens read
  /// from `greens`, a commit it did not bring from `repository`, its trees and blobs from `objects`
  /// (A-638's `PushedObjects`), each work built within `room` bytes and appended through `appends`.
  pub fn new(
    greens: &'w G,
    repository: &'w R,
    objects: &'w O,
    appends: &'w mut A,
    name: [u8; 32],
    room: u64,
  ) -> Self {
    Self {
      greens,
      repository,
      objects,
      appends,
      name,
      room,
    }
  }

  /// An update's work against the version whose head is its old value.
  fn update(
    &self,
    command: &Command,
    pushed: &Pushed<'_>,
    read: GreenRead<'_>,
  ) -> Result<Entry, Declined> {
    let base = self
      .greens
      .version_of(read.id, &command.old)
      .ok_or_else(|| moved(&command.name))?;
    let increment = self.work(command, pushed, read.green, Some(base))?;
    Ok(Entry::Work {
      green: read.id,
      position: read.position,
      increment,
    })
  }

  /// A create's work, against the version it is made at, or over an empty green (module docs).
  fn create(&self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    let parent = self.first_parent(&command.new, pushed);
    let (from, increment) = match self.greens.base_of_create(parent.as_ref()) {
      Some((read, version)) => (
        Some((read.id, version.number)),
        self.work(command, pushed, read.green, Some(version))?,
      ),
      None => (None, self.work(command, pushed, &Green::new(), None)?),
    };
    Ok(Entry::Create {
      name: command.name.clone(),
      from,
      increment,
    })
  }

  /// The first parent of the commit `head` names, through an annotated tag: none for a root commit,
  /// a tree, or an object neither the push nor the repository holds.
  fn first_parent(&self, head: &ObjectId, pushed: &Pushed<'_>) -> Option<ObjectId> {
    let format = head.format();
    let mut scratch = Admission::new(u64::MAX);
    let (kind, content) = self.object(head, pushed)?;
    let target = match kind {
      Kind::Tag => Tag::parse(content, &mut scratch).ok()?.object(format)?,
      _ => *head,
    };
    let (kind, content) = self.object(&target, pushed)?;
    if kind != Kind::Commit {
      return None;
    }
    Commit::parse(content, &mut scratch)
      .ok()?
      .parents(format)
      .next()
  }

  /// A delete: the green retired, its old value its head.
  fn retire(&self, command: &Command, read: GreenRead<'_>) -> Result<Entry, Declined> {
    let at_head = self
      .greens
      .version_of(read.id, &command.old)
      .is_some_and(|version| version.number == read.green.head());
    if !at_head {
      return Err(moved(&command.name));
    }
    Ok(Entry::Retire {
      green: read.id,
      position: read.position,
    })
  }

  /// A-618's work moving `green` from `base` (none for a create) to the tree `command`'s new value
  /// names, the head set to that value.
  fn work(
    &self,
    command: &Command,
    pushed: &Pushed<'_>,
    green: &Green,
    base: Option<Version>,
  ) -> Result<Increment, Declined> {
    let tree = self.tree_of(&command.new, pushed, &command.name)?;
    let base_tree = match base {
      Some(version) => Some(key(&version.tree, &command.name)?),
      None => None,
    };
    let push = Push {
      name: self.name,
      base_version: base.map_or(green.head(), |version| version.number),
      base_tree,
      tree: key(&tree, &command.name)?,
      head: Some(command.new.as_bytes()),
    };
    build(&push, green, self.objects, self.room)
      .map(|built| built.increment)
      .map_err(|refused| work_refused(&command.name, &refused))
  }

  /// The tree the object `head` names: a commit's, or that of the commit or tree an annotated tag
  /// names.
  fn tree_of(
    &self,
    head: &ObjectId,
    pushed: &Pushed<'_>,
    name: &[u8],
  ) -> Result<ObjectId, Declined> {
    let format = head.format();
    let (kind, content) = self.object(head, pushed).ok_or_else(|| missing(name))?;
    let mut scratch = Admission::new(u64::MAX);
    let (kind, target) = match kind {
      Kind::Tag => {
        let tag = Tag::parse(content, &mut scratch).map_err(|_| missing(name))?;
        let target = tag.object(format).ok_or_else(|| missing(name))?;
        (tag.kind(), target)
      }
      kind => (Some(kind), *head),
    };
    match kind {
      Some(Kind::Commit) => {
        let (_, content) = self.object(&target, pushed).ok_or_else(|| missing(name))?;
        let commit = Commit::parse(content, &mut scratch).map_err(|_| missing(name))?;
        commit.tree(format).ok_or_else(|| missing(name))
      }
      Some(Kind::Tree) => Ok(target),
      _ => Err(declined(
        NO_TREE,
        name,
        b"names neither a commit nor a tree",
      )),
    }
  }

  /// The object named `id`: the push's, or the repository's.
  fn object<'p>(&'p self, id: &ObjectId, pushed: &'p Pushed<'p>) -> Option<(Kind, &'p [u8])> {
    match pushed.object(id) {
      Some(object) => Some((object.kind, object.content.as_slice())),
      None => self.repository.object(id),
    }
  }
}

impl<G, R, O, A> Judge for Works<'_, G, R, O, A>
where
  G: Greens + ?Sized,
  R: Repository + ?Sized,
  O: PushObjects,
  A: Appends + ?Sized,
{
  type Accepted = Entry;

  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    let read = self.greens.read(&command.name);
    match (read, command.is_create(), command.is_delete()) {
      (None, true, false) => self.create(command, pushed),
      (Some(read), false, true) => self.retire(command, read),
      (Some(read), false, false) => self.update(command, pushed, read),
      _ => Err(moved(&command.name)),
    }
  }

  fn append(&mut self, command: &Command, entry: Entry) -> Result<Vec<Reported>, Declined> {
    self
      .appends
      .append(entry)
      .map(|()| Vec::new())
      .map_err(|unappended| not_appended(FAILED_TO_UPDATE, &[command.name.as_slice()], &unappended))
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, Entry)>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    let names: Vec<&[u8]> = accepted
      .iter()
      .map(|&(command, _)| command.name.as_slice())
      .collect();
    let entries = accepted.into_iter().map(|(_, entry)| entry).collect();
    self
      .appends
      .append_atomic(entries)
      .map(|()| Vec::new())
      .map_err(|unappended| not_appended(ATOMIC_TRANSACTION_FAILED, &names, &unappended))
  }
}

/// `id` as A-618 keys objects.
fn key(id: &ObjectId, name: &[u8]) -> Result<ObjectKey, Declined> {
  ObjectKey::new(id.as_bytes()).ok_or_else(|| missing(name))
}

/// A refusal for `reason`, its band-2 line naming the ref `name` and `detail`.
fn declined(reason: &[u8], name: &[u8], detail: &[u8]) -> Declined {
  Declined {
    reason: reason.to_vec(),
    hook: Vec::new(),
    message: Some(message(&[name, b": ", detail].concat())),
  }
}

/// The refusal of a ref that moved since the client read it, as git words it.
fn moved(name: &[u8]) -> Declined {
  declined(
    FAILED_TO_UPDATE,
    name,
    b"cannot lock ref: it is not at the value the push read",
  )
}

/// The refusal of a ref whose objects the repository cannot give, as git words it.
fn missing(name: &[u8]) -> Declined {
  declined(MISSING_OBJECTS, name, b"an object it names cannot be read")
}

/// The refusal of an append, `reason` for every command, the band-2 line naming the ref of the
/// entry refused among `names`.
fn not_appended(reason: &[u8], names: &[&[u8]], unappended: &Unappended) -> Declined {
  let (at, detail): (usize, &[u8]) = match unappended {
    Unappended::Moved { at } => (*at, b"cannot lock ref: it moved since the push read it"),
    Unappended::Conflict { at, .. } => (*at, b"cannot lock ref: another push came first"),
  };
  let name = names.get(at).copied().unwrap_or_default();
  declined(reason, name, detail)
}

/// A-618's refusal of a ref's work, in git's words where git has them and slates' otherwise.
fn work_refused(name: &[u8], refused: &PushRefused) -> Declined {
  let (reason, path): (&[u8], &[u8]) = match refused {
    PushRefused::UnknownBase { .. } | PushRefused::Compose(_) => return moved(name),
    PushRefused::Object { path, .. } => (MISSING_OBJECTS, path.as_bytes()),
    PushRefused::NameNotHeld { path } => (NAME_NOT_HELD, path.as_slice()),
    PushRefused::DuplicateName { path } => (DUPLICATE_NAME, path.as_bytes()),
    PushRefused::TargetNotHeld { path } => (TARGET_NOT_HELD, path.as_bytes()),
    PushRefused::KindChanged { path } => (KIND_CHANGED, path.as_bytes()),
    PushRefused::PastRoom { .. } => (PAST_ROOM, b"".as_slice()),
    PushRefused::Unplaced { path } => (UNPLACED, path.as_bytes()),
  };
  declined(reason, name, path)
}
