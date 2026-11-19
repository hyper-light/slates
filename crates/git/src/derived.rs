//! git's objects of a version as derived data: its trees and its commit (A-664; A-645 "Derived data, one mechanism";
//! A-647; §4.17.1 "A push is a work" items 3 and 4; §4.17.2 "The repository in git's terms" item 5 and "Pieces (git)"
//! piece 13).
//!
//! **Why.** A work the server turns into the tool's objects (an agent's work through the SDK or a mount; later a merged
//! review request or a server-side rebase) carries no head operation, or two such works on one version would conflict
//! on it: the server makes the commit after the verdict, records it with the version, its time and author taken from
//! the merge record and never from a holder's clock, and every holder recomputes it from the record (§4.17.1 item 3;
//! §4.16 "Apply on holders"). That is how native work becomes visible to stock git clients. So a version's git objects
//! are a pure function of the history, and under A-647 each is a type of derived data ([`slates_vcs::derived`]): its
//! value is computed from the version's change and its parents' values of the same type, kept immutable under (type,
//! version), and the same on every server and tier that computes it.
//!
//! - [`tree`]: [`Trees`], a version's trees. Derived from the version's map of paths to contents (the change the path
//!   map reads, [`slates_vcs::derived::paths`]): each file's mode from its class ([`FileClass`]), entries in git's tree
//!   order, and each directory's tree object made again only when the change touched it, every other subtree taken
//!   from the parent's value by identity.
//! - [`commit`]: [`Commits`], a version's commit. A version a stock client pushed has the client's own commit; one the
//!   server makes has its tree, its parents' commits, author and committer from the merge record's principal identity
//!   ([`Identity`], the principal's record, §4.13), the date from the merge record in UTC and the message from the
//!   submit.
//!
//! **Determinism.** Neither type reads a clock, randomness or anything but the history and the store, and every node
//! is a function of the objects it describes and the store's window, so a value has one identity everywhere. git names
//! each object by a hash of its bytes, so any difference between two holders would be a different commit; the tests
//! hold both types to stock git's own bytes and to a recomputation from scratch, and build every value twice.
//!
//! **What a made object may hold.** Every tree and commit the server makes passes git's strict fsck
//! ([`crate::objects::fsck`]), the check a client that verifies what it fetches (`transfer.fsckObjects`) runs: a
//! version whose files git cannot hold (a `.git` name, a `.gitmodules` symbolic link, a name past 4096 bytes) or a
//! message holding a NUL is refused by kind rather than made into an object a stock client would refuse.
//!
//! **Evidence** (`tests/derived`): golden vectors stock git 2.50.1 wrote (`git write-tree` over `git update-index
//! --cacheinfo`, `git mktree`, `git commit-tree`, `fixtures/made`); an oracle over generated histories holding every
//! version's derived trees to the trees built from the version's whole path map from scratch, identity for identity,
//! and to a naive writer's ids, each derivation's directories and hashed blobs counted against its change's own; and a
//! determinism gate that derives every history twice, eagerly and lazily in another order, in two stores, and compares
//! every node byte for byte.
//!
//! **Owed** (GAPS, the §4.17 entry): the green's state as the path map these types read (A-647's owed map of a green's
//! state, its classes [`FileClass`]'s); recording the made commit with the version and admitting it to the
//! repository's git store (A-664 part 3, over A-662); a submit refused before the verdict when its post-state is one git
//! cannot hold; a `.gitmodules` blob's own checks; a blob's id read from the store's rows by its content, so a rename or
//! a revert hashes nothing; Mercurial's changesets (§4.17.3 decision 7) and Subversion's revisions after these.
//!
//! No I/O, no clock, no randomness, no panic.

pub mod commit;
pub mod tree;

pub use commit::{
  Authored, CommitCounters, CommitSource, Commits, CommitsRefused, Fields, Identity, IdentityPart,
  IdentityRefused,
};
pub use tree::{Directory, Item, TreeCounters, Trees, TreesRefused};

use crate::objects::tree::Mode;

#[cfg(test)]
#[path = "tests/derived/mod.rs"]
mod tests;

/// A file's class in a version's map of paths to contents, as git reads it (§4.17.2 "The repository in git's terms"
/// item 5): the kind of file the version holds at a path, which is its mode in a tree. A directory is no class: the map
/// holds files, and a directory stands wherever a file is beneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
  /// A regular file: `100644`.
  File,
  /// An executable regular file: `100755`.
  Executable,
  /// A symbolic link, whose content is its target: `120000`.
  Symlink,
  /// A submodule's commit, the special node: its content is the commit's id, the repository's hash length of raw bytes,
  /// `160000`.
  Gitlink,
}

/// Format: each class, the byte a version's map keeps it as, and its mode in a tree.
const CLASSES: [(FileClass, u8, Mode); 4] = [
  (FileClass::File, 0, Mode::File),
  (FileClass::Executable, 1, Mode::Executable),
  // Format: the classes continue (the doc above).
  (FileClass::Symlink, 2, Mode::Symlink),
  (FileClass::Gitlink, 3, Mode::Gitlink),
];

impl FileClass {
  /// Format: every class, in the order of their bytes.
  pub const ALL: [FileClass; 4] = [
    FileClass::File,
    FileClass::Executable,
    FileClass::Symlink,
    FileClass::Gitlink,
  ];

  /// The byte a version's map keeps the class as ([`slates_vcs::derived::paths::Entry::class`]).
  pub fn class(self) -> u8 {
    CLASSES
      .iter()
      .find(|(class, _, _)| *class == self)
      .map_or(u8::MAX, |(_, byte, _)| *byte)
  }

  /// The class a map's byte names, if any.
  pub fn from_class(byte: u8) -> Option<FileClass> {
    CLASSES
      .iter()
      .find(|(_, known, _)| *known == byte)
      .map(|(class, _, _)| *class)
  }

  /// The mode git writes for the class in a tree.
  pub fn mode(self) -> Mode {
    CLASSES
      .iter()
      .find(|(class, _, _)| *class == self)
      .map_or(Mode::File, |(_, _, mode)| *mode)
  }
}
