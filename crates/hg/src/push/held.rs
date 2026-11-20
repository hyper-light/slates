//! A push's [`Held`] over the repository's index (A-657; §4.17.3 "Push" step 2 and "The repository's index"): each
//! log's full texts by node, read from the index's rows (A-588 part 2) and the content store their contents live in,
//! so a push is verified against exactly what the repository holds.
//!
//! **A text.** A changeset's and a manifest's text is its row's one content, read whole (an object longer than a window
//! through its lists, `ContentStore::object_bytes`) and held to the row's length. A file revision's row keeps its bytes
//! and its metadata block apart, so its text is put back together as 7.2 stores it \[C\]: `filelog.py`: the block
//! between its two markers, then the bytes, whose length is the row's less the block's and the markers'.
//!
//! **What is held.** A node is held where its log's node row names a revision whose own row holds that node and whose
//! contents the store holds whole; anything else is not held, so verification refuses a delta against it (7.2's
//! "unknown delta base") rather than read a text the store no longer holds.

use std::borrow::Cow;

use slates_vcs::ContentStore;

use super::verify::Held;
use crate::Node;
use crate::index::{HgIndex, Log};

/// What a repository holds: its index, and the store its rows' contents live in.
#[derive(Clone, Copy)]
pub struct IndexHeld<'a> {
  /// The index.
  index: &'a HgIndex,
  /// The store holding its rows' contents.
  store: &'a ContentStore,
}

impl<'a> IndexHeld<'a> {
  /// What `index` holds, its rows' contents in `store`.
  pub fn new(index: &'a HgIndex, store: &'a ContentStore) -> Self {
    IndexHeld { index, store }
  }

  /// The text of `node` in the manifest log or a file log: a manifest's text, or a file revision's block and bytes.
  fn revision_text(&self, log: Log<'_>, node: &Node) -> Option<Vec<u8>> {
    let rev = self.index.rev(self.store, log, node)?;
    let row = self.index.revision(self.store, log, rev)?;
    if row.node != *node {
      return None;
    }
    self.index.revision_text(self.store, log, rev)
  }
}

impl Held for IndexHeld<'_> {
  fn changeset_text(&self, node: &Node) -> Option<Cow<'_, [u8]>> {
    let rev = self.index.rev(self.store, Log::Changelog, node)?;
    let row = self.index.changeset(self.store, rev)?;
    if row.node != *node {
      return None;
    }
    self.store.object_bytes(&row.text, row.len).map(Cow::Owned)
  }

  fn manifest_text(&self, node: &Node) -> Option<Cow<'_, [u8]>> {
    self.revision_text(Log::Manifest, node).map(Cow::Owned)
  }

  fn file_text(&self, path: &[u8], node: &Node) -> Option<Cow<'_, [u8]>> {
    self.revision_text(Log::File(path), node).map(Cow::Owned)
  }
}
