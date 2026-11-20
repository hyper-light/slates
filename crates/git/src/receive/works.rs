//! receive-pack's part 2 (A-620; §4.17.1 "A push is a work" items 1 to 5 and the namespace's items
//! 4 to 6; §4.17.2 "A push" items 6 and 7; A-642, A-645): each command's update made a work
//! through A-618's [`build`], read through the store's view of the push (A-638's `PushedObjects`),
//! and appended to its green's own log through [`Appends`].
//!
//! **The verdict reads, and moves nothing** ([`Verdicts`]; [`Works`] pairs them with the appends
//! as receive-pack's [`Judge`], and a server whose greens and appends are one shard's state judges
//! through them between its own appends, A-654). Each green is read as the push found it
//! ([`Greens`]), its state immutable at that position, so its work can be computed anywhere (A-645:
//! merges computed anywhere, committed by position):
//! - an update's base is the version whose head is the command's old value (§4.17.1 item 1); a
//!   value no version of the green held is answered as git answers a ref that moved, before any
//!   work is built;
//! - its work is A-618's: the whole-file operations between that version's tree and the tree the
//!   new value names, and the head operation (items 2 and 3);
//! - a create is made at the version of a green its new value's first parent heads, where one can
//!   still be branched from, else at the default branch's head ([`Greens::base_of_create`]), and
//!   its work states the whole difference against that version's tree, so the new green holds
//!   exactly the client's tree; in a repository whose default branch is unborn, over an empty
//!   green. A create of a ref that names a green is a ref that moved, as git's lock refuses a ref
//!   that exists (A-654, main's ruling of 2026-11-19);
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

use slates_merge::pace::Pace;
use slates_merge::push::{Building, Built, ObjectKey, Push, PushObjects, PushRefused, build};
use slates_merge::{ConflictWindow, Green, Increment};
use std::cell::Cell;

use super::{
  ATOMIC_TRANSACTION_FAILED, Command, Declined, FAILED_TO_UPDATE, Judge, MISSING_OBJECTS, Pushed,
  Reported, Repository, message,
};
use std::borrow::Cow;

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
/// green). A green is lent for one call at a time, as the server lends one member of a lineage at a
/// time (A-575), so each read runs its caller's step over the green and answers what it made.
pub trait Greens {
  /// Runs `with` over the green the ref `name` names, read for this push: what it made, or none
  /// for a ref that names none.
  fn read<T>(&self, name: &[u8], with: impl FnOnce(GreenRead<'_>) -> T) -> Option<T>;

  /// The version of `green` whose head is `head`, if one held it (§4.17.1 item 1: a push's base).
  fn version_of(&self, green: u64, head: &ObjectId) -> Option<Version>;

  /// Runs `with` over the green and version a create whose new value's first parent is `parent` is
  /// made at (module docs): the version whose head is `parent` where one can still be branched
  /// from; else the default branch's head. What it made, or none while the default branch is
  /// unborn.
  fn base_of_create<T>(
    &self,
    parent: Option<&ObjectId>,
    with: impl FnOnce(GreenRead<'_>, Version) -> T,
  ) -> Option<T>;
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
  /// The append refused it for a reason of its own, given in slates' words (the shard's budget, the
  /// register's capacity, an approval the branch requires): git's words for a ref not updated, with
  /// `why` on band 2 (A-654).
  Refused {
    /// The entry's place.
    at: usize,
    /// Why, in slates' words.
    why: Vec<u8>,
  },
}

/// The appends that move refs (A-642: each green its own log; module docs).
pub trait Appends {
  /// Appends one entry on its green's own log.
  fn append(&mut self, entry: Entry) -> Result<(), Unappended>;

  /// Appends an atomic push's entries, each on its green's own log: every one or none.
  fn append_atomic(&mut self, entries: Vec<Entry>) -> Result<(), Unappended>;
}

/// A push's own objects of any kind, as its quarantine holds them (A-719 part 3): what a verdict
/// reads a pushed commit or tag from, the session no longer holding the push's bytes.
pub trait PushedBytes {
  /// The object named `id`, its type and bytes, from the index first and then the push.
  fn pushed_object(&self, id: &ObjectId) -> Option<(Kind, Cow<'_, [u8]>)>;
}

/// receive-pack's part 2 verdicts, read only (module docs): each command's entry, judged over the
/// greens as the push read them, moving nothing. [`Works`] pairs them with an [`Appends`]; a server
/// whose greens and appends are one shard's state judges through them between its own appends
/// (A-654), each borrow of the state ending with its call.
pub struct Verdicts<'w, G: ?Sized, R: ?Sized, O> {
  /// The greens as the push reads them.
  greens: &'w G,
  /// The repository's indexed objects, for a commit or tag the push did not bring.
  repository: &'w R,
  /// The push's trees and blobs, the index first and then the push's quarantine.
  objects: &'w O,
  /// The push's name, which every work's identity begins with (A-618).
  name: [u8; 32],
  /// The room each work is built within.
  room: u64,
  /// A command's work built over the turns before its verdict ([`Verdicts::prepare`]), which its verdict takes.
  prepared: Cell<Option<Preparing>>,
}

/// A command's work built a unit at a time over the turns before its verdict (A-752; §4.3's step budget): a push over
/// a head of many files builds its work over as many turns as it needs ([`Verdicts::prepare`], from a judge's
/// [`Judge::ready`]), and its verdict takes the work built ([`Verdicts::prepared`]) rather than building it in one step.
pub struct Preparing {
  /// The ref and the value it is built for.
  command: (Vec<u8>, ObjectId),
  /// The build.
  building: Building,
  /// What it built, once done.
  done: Option<Result<Built, PushRefused>>,
  /// A create's first parent, which names the green it is made at: read once, at its first turn.
  parent: Option<ObjectId>,
}

impl Preparing {
  /// Its build's longest unit so far, nanoseconds on its owner's clock (A-752).
  pub fn longest_unit_ns(&self) -> u64 {
    self.building.pace().longest_unit_ns()
  }

  /// Its last turn's opening, nanoseconds on its owner's clock: what the turn did before its first unit.
  pub fn opening_ns(&self) -> u64 {
    self.building.pace().opening_ns()
  }

  /// When its last turn last asked for a unit, on its owner's clock: what the turn did after is its owner's.
  pub fn asked_at_ns(&self) -> u64 {
    self.building.pace().asked_at_ns()
  }
}

impl core::fmt::Debug for Preparing {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    f.debug_struct("Preparing")
      .field("command", &self.command)
      .field("done", &self.done.is_some())
      .finish_non_exhaustive()
  }
}

/// receive-pack's part 2 judge (module docs): the verdicts, appended through `appends`.
pub struct Works<'w, G: ?Sized, R: ?Sized, O, A: ?Sized> {
  /// The verdicts.
  verdicts: Verdicts<'w, G, R, O>,
  /// Where the works are appended.
  appends: &'w mut A,
}

impl<'w, G, R, O, A> Works<'w, G, R, O, A>
where
  G: Greens + ?Sized,
  R: Repository + ?Sized,
  O: PushObjects + PushedBytes,
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
      verdicts: Verdicts::new(greens, repository, objects, name, room),
      appends,
    }
  }
}

impl<'w, G, R, O> Verdicts<'w, G, R, O>
where
  G: Greens + ?Sized,
  R: Repository + ?Sized,
  O: PushObjects + PushedBytes,
{
  /// The verdicts of the push named `name` (two submits of one push carry one name): its greens
  /// read from `greens`, a commit it did not bring from `repository`, its trees and blobs from
  /// `objects` (A-638's `PushedObjects`), each work built within `room` bytes.
  pub fn new(greens: &'w G, repository: &'w R, objects: &'w O, name: [u8; 32], room: u64) -> Self {
    Self {
      greens,
      repository,
      objects,
      name,
      room,
      prepared: Cell::new(None),
    }
  }

  /// These verdicts, taking the work `prepared` built for its command, where it is done and still that command's.
  pub fn prepared(self, prepared: Option<Preparing>) -> Self {
    self.prepared.set(prepared);
    self
  }

  /// One turn of `command`'s work, built against the green and base its verdict will read (A-752): whether it is
  /// ready, so this step judges it (a delete, a command whose verdict refuses before any work, or a work built); not
  /// ready, the turn, begun at `began`, ended at `deadline` on `clock` (the owner's, nanoseconds) and `preparing` keeps
  /// the build for the next. What the turn did before its first unit is its opening ([`Preparing::opening_ns`]). A build for another command or push is begun again.
  pub fn prepare(
    &self,
    command: &Command,
    pushed: &Pushed<'_>,
    preparing: &mut Option<Preparing>,
    (clock, began, deadline): (fn() -> u64, u64, u64),
  ) -> bool {
    if command.is_delete() {
      return true;
    }
    let ours = preparing.as_ref().is_some_and(|prepared| {
      prepared.command.0 == command.name && prepared.command.1 == command.new
    });
    if !ours {
      *preparing = None;
    }
    if preparing
      .as_ref()
      .is_some_and(|prepared| prepared.done.is_some())
    {
      return true;
    }
    // A build under way continues over the green it began on: its push, read at its first turn, stands.
    let begun = preparing.is_some();
    let parent = match preparing.as_ref() {
      Some(prepared) => prepared.parent,
      None if command.is_create() => self.first_parent(&command.new, pushed),
      None => None,
    };
    let mut turn = |green: &Green, base: Option<Version>| -> bool {
      if !begun {
        let Ok(push) = self.push_of(command, pushed, green, base) else {
          return true;
        };
        *preparing = Some(Preparing {
          command: (command.name.clone(), command.new),
          building: Building::new(&push, self.room, Pace::new(clock)),
          done: None,
          parent,
        });
      }
      let Some(prepared) = preparing.as_mut() else {
        return true;
      };
      let pace = prepared.building.pace();
      pace.turn_from(began, deadline);
      prepared.done = prepared.building.advance(green, self.objects);
      prepared.done.is_some()
    };
    if command.is_create() {
      if self.greens.read(&command.name, |_| ()).is_some() {
        return true;
      }
      return self
        .greens
        .base_of_create(parent.as_ref(), |read, version| {
          turn(read.green, Some(version))
        })
        .unwrap_or_else(|| turn(&Green::new(), None));
    }
    self
      .greens
      .read(&command.name, |read| {
        if begun {
          return turn(read.green, None);
        }
        match self.greens.version_of(read.id, &command.old) {
          Some(base) => turn(read.green, Some(base)),
          None => true,
        }
      })
      .unwrap_or(true)
  }

  /// `command`'s entry (module docs): an update's work, a create's new green, a delete's
  /// retirement, or why not, in git's words where git has them.
  pub fn judge(&self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    let (create, delete) = (command.is_create(), command.is_delete());
    let judged = self
      .greens
      .read(&command.name, |read| match (create, delete) {
        (false, true) => self.retire(command, read),
        (false, false) => self.update(command, pushed, read),
        _ => Err(moved(&command.name)),
      });
    match (judged, create, delete) {
      (Some(judged), ..) => judged,
      (None, true, false) => self.create(command, pushed),
      _ => Err(moved(&command.name)),
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
    let made = self
      .greens
      .base_of_create(parent.as_ref(), |read, version| {
        self
          .work(command, pushed, read.green, Some(version))
          .map(|increment| (Some((read.id, version.number)), increment))
      });
    let (from, increment) = match made {
      Some(made) => made?,
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
      Kind::Tag => Tag::parse(&content, &mut scratch).ok()?.object(format)?,
      _ => *head,
    };
    let (kind, content) = self.object(&target, pushed)?;
    if kind != Kind::Commit {
      return None;
    }
    Commit::parse(&content, &mut scratch)
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
    let push = self.push_of(command, pushed, green, base)?;
    let prepared = self
      .prepared
      .take()
      .filter(|prepared| prepared.building.is_of(&push))
      .and_then(|prepared| prepared.done);
    prepared
      .unwrap_or_else(|| build(&push, green, self.objects, self.room))
      .map(|built| built.increment)
      .map_err(|refused| work_refused(&command.name, &refused))
  }

  /// The push `command` makes of `green` from `base` (none for a create over an empty green): the tree its new value
  /// names over the base's.
  fn push_of<'c>(
    &self,
    command: &'c Command,
    pushed: &Pushed<'_>,
    green: &Green,
    base: Option<Version>,
  ) -> Result<Push<'c>, Declined> {
    let tree = self.tree_of(&command.new, pushed, &command.name)?;
    let base_tree = match base {
      Some(version) => Some(key(&version.tree, &command.name)?),
      None => None,
    };
    Ok(Push {
      name: self.name,
      base_version: base.map_or(green.head(), |version| version.number),
      base_tree,
      tree: key(&tree, &command.name)?,
      head: Some(command.new.as_bytes()),
    })
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
        let tag = Tag::parse(&content, &mut scratch).map_err(|_| missing(name))?;
        let target = tag.object(format).ok_or_else(|| missing(name))?;
        (tag.kind(), target)
      }
      kind => (Some(kind), *head),
    };
    match kind {
      Some(Kind::Commit) => {
        let (_, content) = self.object(&target, pushed).ok_or_else(|| missing(name))?;
        let commit = Commit::parse(&content, &mut scratch).map_err(|_| missing(name))?;
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

  /// The object named `id`: the push's, as its pushed objects lend it or as its quarantine holds it
  /// (A-719 part 3: a server keeps the push's bytes in its store, not in the session), or the
  /// repository's.
  fn object<'p>(&'p self, id: &ObjectId, pushed: &Pushed<'p>) -> Option<(Kind, Cow<'p, [u8]>)> {
    if let Some(object) = pushed.object(id) {
      return Some(object);
    }
    if let Some(object) = self.objects.pushed_object(id) {
      return Some(object);
    }
    self
      .repository
      .object(id)
      .map(|(kind, content)| (kind, Cow::Borrowed(content)))
  }
}

impl<G, R, O, A> Judge for Works<'_, G, R, O, A>
where
  G: Greens + ?Sized,
  R: Repository + ?Sized,
  O: PushObjects + PushedBytes,
  A: Appends + ?Sized,
{
  type Accepted = Entry;

  fn judge(&mut self, command: &Command, pushed: &Pushed<'_>) -> Result<Entry, Declined> {
    self.verdicts.judge(command, pushed)
  }

  fn append(&mut self, command: &Command, entry: Entry) -> Result<Vec<Reported>, Declined> {
    self
      .appends
      .append(entry)
      .map(|()| Vec::new())
      .map_err(|unappended| append_declined(&command.name, &unappended))
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
      .map_err(|unappended| atomic_declined(&names, &unappended))
  }
}

/// The refusal of the ref `name`'s entry the append did not take, in git's words for a ref not
/// updated (`failed to update ref`), why on band 2.
pub fn append_declined(name: &[u8], unappended: &Unappended) -> Declined {
  not_appended(FAILED_TO_UPDATE, &[name], unappended)
}

/// The refusal of an atomic push's entries the append did not take, in git's words (`atomic
/// transaction failed`) for every command, the band-2 line naming the ref refused among `names`.
pub fn atomic_declined(names: &[&[u8]], unappended: &Unappended) -> Declined {
  not_appended(ATOMIC_TRANSACTION_FAILED, names, unappended)
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
    Unappended::Refused { at, why } => (*at, why.as_slice()),
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
