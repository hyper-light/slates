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

use super::verify::{FileNames, Held};
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

  /// The content store keeps each file revision's bytes as an object of their own, its metadata block apart (the
  /// intake's `bytes_from`; A-767 C1).
  fn stores_bytes(&self) -> bool {
    true
  }

  /// The row's own names (A-767 C1): its block's and its bytes' identities as the intake kept them, no text read.
  fn file_names(&self, path: &[u8], node: &Node) -> Option<FileNames> {
    let log = Log::File(path);
    let rev = self.index.rev(self.store, log, node)?;
    let row = self.index.revision(self.store, log, rev)?;
    (row.node == *node).then_some(FileNames {
      block: row.block,
      bytes: row.bytes,
    })
  }

  /// The row's own length (A-767 C1): its text's, less its metadata block's whole, which the store keeps whether or
  /// not the text's bytes are at hand; no text read. The block the row names is what lies between the text's two
  /// markers (`filerev::split`), so a text with a block, empty or not, spends its length and both markers' on it (an
  /// empty block's four bytes were once counted as bytes: `LengthDiffers { said: 10101, given: 10097 }`, w-interop).
  fn file_bytes_len(&self, path: &[u8], node: &Node) -> Option<u64> {
    let log = Log::File(path);
    let rev = self.index.rev(self.store, log, node)?;
    let row = self.index.revision(self.store, log, rev)?;
    if row.node != *node {
      return None;
    }
    let block = match row.block {
      Some(block) => self.store.len_of(&block)?.checked_add(crate::read::to_u64(
        crate::filerev::MARKER.len().saturating_mul(2),
      ))?,
      None => 0,
    };
    row.len.checked_sub(block)
  }
}

/// A file revision the repository holds whose text a reader waits for, asked of the node's changeset store (A-767 C1: a
/// text an import or a push let go): its row, then its whole text within `most` bytes. A push's work asks it between its
/// turns (`crate::push::tree::WorkBuild::ask_waited`), and a home brings a let-go revision back by it (goal 93 item
/// 2b(b)); its owner answers it ([`crate::render::StoreWants`]), then takes the text ([`TextAsk::taken`]).
pub struct TextAsk {
  /// The file's path.
  path: Vec<u8>,
  /// The revision's node.
  node: Node,
  /// What is asked next.
  want: Option<crate::render::Want>,
  /// The most bytes its text may hold.
  most: u64,
  /// Its text, once given.
  text: Option<Vec<u8>>,
}

impl TextAsk {
  /// The ask of the text of the revision `node` of `path`'s file log, within `most` bytes: its row first.
  pub fn new(path: Vec<u8>, node: Node, most: u64) -> Self {
    TextAsk {
      want: Some(crate::render::Want::Rows {
        logs: vec![(path.clone(), vec![node])],
      }),
      path,
      node,
      most,
      text: None,
    }
  }

  /// The text the store gave, with its path and node; none where the store kept no such revision or gave no text.
  pub fn taken(self) -> Option<(Vec<u8>, Node, Vec<u8>)> {
    let text = self.text?;
    Some((self.path, self.node, text))
  }
}

impl crate::render::StoreWants for TextAsk {
  fn wants(&self) -> Option<&crate::render::Want> {
    self.want.as_ref()
  }

  fn give_rows(&mut self, rows: Vec<Vec<Option<crate::render::StoredRevision>>>) {
    self.want = rows
      .into_iter()
      .next()
      .and_then(|log| log.into_iter().next())
      .flatten()
      .map(|row| crate::render::Want::Texts {
        texts: vec![row.text],
        most: self.most,
      });
  }

  fn give_chunks(&mut self, _chunks: Vec<Vec<u8>>) {
    self.want = None;
  }

  fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    self.want = None;
    self.text = texts.into_iter().next();
  }
}

/// The length of the text of the revision `node` of `path`'s file log that `index` holds, and whether `store` holds its
/// metadata block and its bytes whole ([`ContentStore::holds_whole`]): what a home reads before it brings a let-go
/// revision back (goal 93 item 2b(b)). None where the index holds no such revision.
pub fn file_text_held(
  (index, store): (&HgIndex, &ContentStore),
  (path, node): (&[u8], &Node),
) -> Option<(u64, bool)> {
  let log = Log::File(path);
  let row = index
    .rev(store, log, node)
    .and_then(|rev| index.revision(store, log, rev))
    .filter(|row| row.node == *node)?;
  let whole =
    store.holds_whole(&row.bytes) && row.block.is_none_or(|block| store.holds_whole(&block));
  Some((row.len, whole))
}

/// What a reader of the changelog's or the manifest log's revision `node` needs to bring it back once its windows were
/// let go (goal 93, C1 (1): these logs' texts off the content store, read through the node's changeset store): its
/// text's length, whether `store` holds the object its row names whole, and for a manifest kept as a delta the node of
/// the revision the delta is against, whose text the delta is made from ([`bring_back_log`]). None where the index holds
/// no such revision, or `log` is a file log ([`file_text_held`]).
pub fn log_text_held(
  (index, store): (&HgIndex, &ContentStore),
  log: Log<'_>,
  node: &Node,
) -> Option<LogTextHeld> {
  let rev = index.rev(store, log, node)?;
  match log {
    Log::Changelog => {
      let row = index
        .changeset(store, rev)
        .filter(|row| row.node == *node)?;
      Some(LogTextHeld {
        len: row.len,
        whole: store.holds_whole(&row.text),
        against: None,
      })
    }
    Log::Manifest => {
      let row = index
        .revision(store, log, rev)
        .filter(|row| row.node == *node)?;
      let against = if row.base == crate::index::NONE {
        None
      } else {
        Some(index.revision(store, log, row.base)?.node)
      };
      Some(LogTextHeld {
        len: row.len,
        whole: store.holds_whole(&row.bytes),
        against,
      })
    }
    Log::File(_) => None,
  }
}

/// A changelog's or a manifest log's revision as [`log_text_held`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogTextHeld {
  /// Its text's length.
  pub len: u64,
  /// Whether the store holds the object its row names whole.
  pub whole: bool,
  /// For a manifest kept as a delta, the node of the revision it is a delta against.
  pub against: Option<Node>,
}

/// Brings back into `store` the changelog's or the manifest log's revision `node` that `index` holds, from `text`, its
/// whole text as its reader read it from the node's changeset store (goal 93, C1 (1)): a changeset's text, or a
/// manifest's whole text, by the identity its row names; a manifest kept as a delta made again from `against`, the text
/// of the revision it is against ([`LogTextHeld::against`]), as the index made it (`kept_as`: line deltas,
/// deterministic), and put back only where it is the very delta the row names.
///
/// # Errors
/// `NotHeld` where the index holds no such revision, `log` is a file log, a delta's `against` text is missing, or the
/// text (or the delta made from it) is not the row's own; the store's refusal (its budget) as a put's.
pub fn bring_back_log(
  (index, store): (&HgIndex, &mut ContentStore),
  log: Log<'_>,
  node: &Node,
  (text, against): (&[u8], Option<&[u8]>),
) -> Result<(), slates_vcs::ContentRefused> {
  let rev = index
    .rev(store, log, node)
    .ok_or(slates_vcs::ContentRefused::NotHeld)?;
  match log {
    Log::Changelog => {
      let row = index
        .changeset(store, rev)
        .filter(|row| row.node == *node)
        .ok_or(slates_vcs::ContentRefused::NotHeld)?;
      let identity = row.text;
      store.bring_back_object(&identity, text).map(drop)
    }
    Log::Manifest => {
      let row = index
        .revision(store, log, rev)
        .filter(|row| row.node == *node)
        .ok_or(slates_vcs::ContentRefused::NotHeld)?;
      if row.base == crate::index::NONE {
        return store.bring_back_object(&row.bytes, text).map(drop);
      }
      let against = against.ok_or(slates_vcs::ContentRefused::NotHeld)?;
      let mut delta = Vec::new();
      crate::delta::encode_lines(against, text, &mut delta)
        .map_err(|_| slates_vcs::ContentRefused::NotHeld)?;
      store.bring_back_object(&row.bytes, &delta).map(drop)
    }
    Log::File(_) => Err(slates_vcs::ContentRefused::NotHeld),
  }
}

/// Brings back into `store` the text of the revision `node` of `path`'s file log that `index` holds, from `text`, the
/// whole text its reader read from the node's changeset store (A-767 C1: a push's work reading a text an import let go):
/// its metadata block and its bytes, each by the identity its row names, each window let go brought back.
///
/// # Errors
/// `NotHeld` where the index holds no such revision or the text is not its own; the store's refusal (its budget) as a
/// put's.
pub fn bring_back_file(
  (index, store): (&HgIndex, &mut ContentStore),
  (path, node): (&[u8], &Node),
  text: &[u8],
) -> Result<(), slates_vcs::ContentRefused> {
  let log = Log::File(path);
  let row = index
    .rev(store, log, node)
    .and_then(|rev| index.revision(store, log, rev))
    .filter(|row| row.node == *node)
    .ok_or(slates_vcs::ContentRefused::NotHeld)?;
  let split = crate::filerev::split(text).map_err(|_| slates_vcs::ContentRefused::NotHeld)?;
  store.bring_back_object(&row.bytes, split.bytes)?;
  if let (Some(block), Some(metadata)) = (row.block, split.metadata) {
    store.bring_back_object(&block, metadata)?;
  }
  Ok(())
}
