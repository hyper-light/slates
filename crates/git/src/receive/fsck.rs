//! Every pushed object checked as git checks a push (A-620; §4.17.2 "A push", item 4: "Check every
//! new object as git's `transfer.fsckObjects` does, always"): A-590's checks of trees, commits, tags,
//! and the `.gitmodules` and `.gitattributes` blobs referenced by trees under a push's strictness
//! ([`Strictness::Strict`], git's `index-pack --strict` and `unpack-objects --strict`), in pack
//! order, the first object refused ending the pack, as git's first fsck error ends its unpacking.

use std::borrow::Cow;

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::{FsckId, Strictness, commit, special, tag, tree};
use crate::unpack::Object;

/// Which special dotfile a tree entry named (git's `gitmodules_found` and `gitattributes_found`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum SpecialKind {
  /// `.gitmodules` (in any spelling HFS+ or NTFS resolves).
  Gitmodules,
  /// `.gitattributes` (in any spelling HFS+ or NTFS resolves).
  Gitattributes,
}

/// One special dotfile object referenced by a pushed tree, to check once the pack is unpacked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct SpecialBlob {
  /// The referenced object's id.
  pub(crate) id: ObjectId,
  /// Which dotfile it was named as.
  pub(crate) kind: SpecialKind,
}

/// The first of `objects`, in pack order, that a push's check refuses, with the finding that
/// refused it; `None` when every one passes. Trees referencing `.gitmodules` or `.gitattributes`
/// also check those blobs against `objects` once all objects in the pack have been walked.
pub fn first_refusal(objects: &[Object], format: ObjectFormat) -> Option<(ObjectId, FsckId)> {
  let mut special_blobs = Vec::new();
  for object in objects {
    if let Some(finding) = check_object(object.kind, &object.content, format, &mut special_blobs) {
      return Some((object.id, finding));
    }
  }
  special_blobs.sort_unstable();
  special_blobs.dedup();
  for entry in special_blobs {
    let lookup = |id: &ObjectId| {
      objects
        .iter()
        .find(|object| &object.id == id)
        .map(|object| (object.kind, Cow::Borrowed(object.content.as_slice())))
    };
    if let Some(finding) = check_special(entry, lookup, |_| false) {
      return Some((entry.id, finding));
    }
  }
  None
}

/// What a push's check finds against an object of `kind` holding `content`, if it refuses it; any
/// `.gitmodules` or `.gitattributes` entries found in a tree are appended to `special_blobs` to be
/// validated once the pack is unpacked.
pub(crate) fn check_object(
  kind: Kind,
  content: &[u8],
  format: ObjectFormat,
  special_blobs: &mut Vec<SpecialBlob>,
) -> Option<FsckId> {
  match kind {
    Kind::Commit => commit::fsck(content, format, Strictness::Strict).refusal(),
    Kind::Tag => tag::fsck(content, format, Strictness::Strict).refusal(),
    Kind::Tree => {
      let mut scratch = Admission::new(u64::MAX);
      match tree::fsck(content, format, Strictness::Strict, &mut scratch) {
        Ok(checked) => {
          if let Some(finding) = checked.report.refusal() {
            return Some(finding);
          }
          for raw in checked.gitmodules {
            if let Some(id) = ObjectId::from_bytes(format, raw) {
              special_blobs.push(SpecialBlob {
                id,
                kind: SpecialKind::Gitmodules,
              });
            }
          }
          for raw in checked.gitattributes {
            if let Some(id) = ObjectId::from_bytes(format, raw) {
              special_blobs.push(SpecialBlob {
                id,
                kind: SpecialKind::Gitattributes,
              });
            }
          }
          None
        }
        Err(_) => Some(FsckId::BadTree),
      }
    }
    Kind::Blob => None,
  }
}

/// Validates one `.gitmodules` or `.gitattributes` object referenced by a tree: looks up its kind
/// and bytes via `lookup` (the pack first, then the repository), and falls back to `already_held`
/// only when the repository already holds a previously validated object whose bytes were let go.
pub(crate) fn check_special<'a>(
  entry: SpecialBlob,
  lookup: impl FnOnce(&ObjectId) -> Option<(Kind, Cow<'a, [u8]>)>,
  already_held: impl FnOnce(&ObjectId) -> bool,
) -> Option<FsckId> {
  let Some((kind, bytes)) = lookup(&entry.id) else {
    if already_held(&entry.id) {
      return None;
    }
    return Some(match entry.kind {
      SpecialKind::Gitmodules => FsckId::GitmodulesMissing,
      SpecialKind::Gitattributes => FsckId::GitattributesMissing,
    });
  };
  if kind != Kind::Blob {
    return Some(match entry.kind {
      SpecialKind::Gitmodules => FsckId::GitmodulesBlob,
      SpecialKind::Gitattributes => FsckId::GitattributesBlob,
    });
  }
  match entry.kind {
    SpecialKind::Gitmodules => special::fsck_gitmodules(&bytes, Strictness::Strict).refusal(),
    SpecialKind::Gitattributes => special::fsck_gitattributes(&bytes, Strictness::Strict).refusal(),
  }
}
