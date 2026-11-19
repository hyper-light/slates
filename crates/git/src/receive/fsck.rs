//! Every pushed object checked as git checks a push (A-620; §4.17.2 "A push", item 4: "Check every
//! new object as git's `transfer.fsckObjects` does, always"): A-590's checks of trees, commits and
//! tags under a push's strictness ([`Strictness::Strict`], git's `index-pack --strict` and
//! `unpack-objects --strict`), in pack order, the first object refused ending the pack, as git's
//! first fsck error ends its unpacking.
//!
//! A blob's own checks, those of the `.gitmodules` and `.gitattributes` blobs a tree names, are
//! A-590's to give (its `FsckId` names them; its checks do not yet read the blobs); until they are,
//! a blob passes here.

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::{FsckId, Strictness, commit, tag, tree};
use crate::unpack::Object;

/// The first of `objects`, in pack order, that a push's check refuses, with the finding that
/// refused it; `None` when every one passes. A tree's check keeps its lists of special entries in a
/// scratch admission of its own: they are a small part of the tree's bytes, which the pack's
/// admission already charged, and are freed before the next object.
pub fn first_refusal(objects: &[Object], format: ObjectFormat) -> Option<(ObjectId, FsckId)> {
  objects
    .iter()
    .find_map(|object| refusal(object, format).map(|finding| (object.id, finding)))
}

/// What a push's check finds against `object`, if it refuses it.
fn refusal(object: &Object, format: ObjectFormat) -> Option<FsckId> {
  let content = object.content.as_slice();
  match object.kind {
    Kind::Commit => commit::fsck(content, format, Strictness::Strict).refusal(),
    Kind::Tag => tag::fsck(content, format, Strictness::Strict).refusal(),
    Kind::Tree => {
      let mut scratch = Admission::new(u64::MAX);
      match tree::fsck(content, format, Strictness::Strict, &mut scratch) {
        Ok(checked) => checked.report.refusal(),
        Err(_) => Some(FsckId::BadTree),
      }
    }
    Kind::Blob => None,
  }
}
