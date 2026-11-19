//! git's naming as the object store's parameter (A-614; §4.17.2 "The repository in git's terms", item 1; §4.17.1
//! "Objects" item 5): an object's id is git's hash of `<type> <length>\0<content>` in the repository's format, SHA-1
//! with collision detection or SHA-256 ([`crate::object_name::name_of`], A-600). An input SHA-1's detection finds to be
//! part of a known collision attack is refused, and the refusal carries git's own words for it.

use slates_vcs::Naming;

use crate::object_name::{CollisionAttack, Kind, ObjectFormat, ObjectId, name_of};

/// Format: git's words for an input its SHA-1 collision detection refused, before the input's plain SHA-1 in
/// hexadecimal (`sha1dc_git.c`, `git_SHA1DCFinal`: "SHA-1 appears to be part of a collision attack: %s").
pub const COLLISION_WORDS: &str = "SHA-1 appears to be part of a collision attack: ";

/// Why git's naming refused bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRefused {
  /// SHA-1's collision detection found the input to be part of a known collision attack, named by its plain SHA-1.
  CollisionAttack(CollisionAttack),
  /// A kind that is none of git's four object types' numbers (`object.h`'s `enum object_type`).
  UnknownKind {
    /// The number given.
    kind: u8,
  },
}

impl NameRefused {
  /// What git says of the refusal: its words for a collision attack, naming the plain SHA-1, and for a type number it
  /// does not know, index-pack's ("unknown object type %d").
  pub fn words(&self) -> String {
    match self {
      NameRefused::CollisionAttack(attack) => format!("{COLLISION_WORDS}{}", attack.sha1),
      NameRefused::UnknownKind { kind } => format!("unknown object type {kind}"),
    }
  }
}

/// The kind a pack's type number names (`object.h`: a commit 1, a tree 2, a blob 3, a tag 4), or `None`.
pub fn kind_of(number: u8) -> Option<Kind> {
  [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag]
    .into_iter()
    .find(|kind| kind.pack_number() == number)
}

/// git's naming in a repository of one format (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitNaming {
  /// The repository's object format.
  pub format: ObjectFormat,
}

impl Naming for GitNaming {
  type Id = ObjectId;
  type Refused = NameRefused;

  fn name(&self, kind: u8, _context: &[u8], bytes: &[u8]) -> Result<ObjectId, NameRefused> {
    let kind = kind_of(kind).ok_or(NameRefused::UnknownKind { kind })?;
    name_of(self.format, kind, bytes).map_err(NameRefused::CollisionAttack)
  }
}
