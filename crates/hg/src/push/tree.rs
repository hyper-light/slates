//! A version's tree in Mercurial's terms, as A-618's builder reads it (A-641 part 2; §4.17.1 "A push is a work" items 2
//! to 4; §4.17.3 decision 6): a flat manifest's files laid out under directories, which Mercurial does not have, each
//! named by what it holds, so the builder compares two versions' trees by identity and passes over every directory they
//! hold alike, as it passes over a git tree whose id did not change.
//!
//! **A directory's name.** BLAKE3 in key-derivation mode under a context of its own, over its entries in the order the
//! manifest lists them: each name's length and bytes, its kind, and its id's length and bytes. Thirty-two bytes, so a
//! directory's key is never a file node's twenty (A-618's `ObjectKey` keeps its length). Two directories holding the
//! same entries are the same lines of their sorted manifests in the same order, so they hash alike; the cost is one pass
//! over each manifest a work compares, the manifest's own decode's order (an index of directories kept with the
//! manifest log would make it the changed paths' alone: owed, with the index's rows of piece 3).
//!
//! **A file.** The blob of a manifest's entry is its file revision's bytes without the metadata block (`filerev`), found
//! in the push's revisions or the repository's by the path that names it; `x` is an executable, `l` a symbolic link
//! whose bytes are its target, as Mercurial's checkout writes them. The head A-618 sets is the changeset's node, its
//! twenty bytes, which the server reads back as the line's head (decision 6).

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_merge::Green;
use slates_merge::push::{self, Built, EntryKind, ObjectKey, Push, PushObjects, PushRefused, TreeEntry, Unavailable};

use super::verify::{Held, Verified, manifest_text};
use crate::manifest::{self, Flag};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, PastAdmission, changeset, filerev};

/// Format: the BLAKE3 key-derivation context a laid-out directory's name is hashed under, so it is never another
/// identity's.
const DIRECTORY_CONTEXT: &str = "slates 2026-11-19 a Mercurial manifest's directory";

/// Format: the separator of a tracked path's components.
const SLASH: u8 = b'/';

/// The path of the root directory: no bytes.
const EMPTY: &[u8] = &[];

/// Format: each entry kind's byte in a directory's hashed form.
const KIND_BYTES: [(EntryKind, u8); 5] = [
  (EntryKind::File, 0),
  (EntryKind::Executable, 1),
  (EntryKind::Symlink, 2),
  // Format: then the two kinds a directory holds that are not files.
  (EntryKind::Directory, 3),
  (EntryKind::Submodule, 4),
];

/// Why a version's tree could not be laid out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutRefusal {
  /// A changeset neither the push brought nor the repository holds.
  MissingChangeset {
    /// The changeset.
    node: Node,
  },
  /// A changeset text 7.2's reader refuses.
  Changeset {
    /// The changeset.
    node: Node,
    /// What the reader found.
    fault: changeset::Refusal,
  },
  /// A manifest neither the push brought nor the repository holds.
  MissingManifest {
    /// The manifest.
    node: Node,
  },
  /// A manifest text 7.2's reader refuses.
  Manifest {
    /// The manifest.
    node: Node,
    /// What the reader found.
    fault: manifest::Refusal,
  },
  /// A directory's name A-618's key cannot hold: never met, every name being thirty-two bytes.
  Unnamed,
  /// More than the request's admission had left.
  PastAdmission(PastAdmission),
}

impl From<PastAdmission> for LayoutRefusal {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Why a push's work could not be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkRefused {
  /// A tree could not be laid out.
  Layout(LayoutRefusal),
  /// A-618's builder refused the work.
  Push(PushRefused),
}

/// Versions' trees laid out from their manifests, the push's revisions over the repository's, as A-618's builder reads
/// them.
pub struct Trees<'v, H: ?Sized> {
  /// What the push brought.
  verified: &'v Verified,
  /// What the repository holds.
  held: &'v H,
  /// Every directory laid out, by its name.
  dirs: BTreeMap<ObjectKey, Vec<TreeEntry>>,
  /// Each file node a laid-out manifest names, with the path naming it: where its revision is found.
  paths: BTreeMap<Node, Vec<u8>>,
}

/// One directory being laid out: its path and its entries so far.
struct Open {
  /// The path, empty for the root.
  path: Vec<u8>,
  /// The entries, in the manifest's order.
  entries: Vec<TreeEntry>,
}

impl<'v, H: Held + ?Sized> Trees<'v, H> {
  /// Nothing laid out yet, over the push's revisions and the repository's.
  pub fn new(verified: &'v Verified, held: &'v H) -> Self {
    Trees {
      verified,
      held,
      dirs: BTreeMap::new(),
      paths: BTreeMap::new(),
    }
  }

  /// Lays out the changeset `changeset`'s tree (module docs): its root directory's name.
  pub fn lay_out_changeset(
    &mut self,
    changeset: &Node,
    admission: &mut Admission,
  ) -> Result<ObjectKey, LayoutRefusal> {
    let text = match self.verified.changeset(changeset) {
      Some(arrived) => Cow::Borrowed(arrived.text.as_slice()),
      None => self
        .held
        .changeset_text(changeset)
        .ok_or(LayoutRefusal::MissingChangeset { node: *changeset })?,
    };
    let manifest = changeset::decode(&text)
      .and_then(|read| read.manifest_node())
      .map_err(|fault| LayoutRefusal::Changeset {
        node: *changeset,
        fault,
      })?;
    self.lay_out(&manifest, admission)
  }

  /// Lays out the manifest `node`'s files as directories: the root directory's name.
  pub fn lay_out(&mut self, node: &Node, admission: &mut Admission) -> Result<ObjectKey, LayoutRefusal> {
    let verified = self.verified;
    let held = self.held;
    let text = manifest_text(verified, held, node).ok_or(LayoutRefusal::MissingManifest { node: *node })?;
    let entries = manifest::decode(&text).map_err(|fault| LayoutRefusal::Manifest { node: *node, fault })?;
    let mut stack = vec![Open {
      path: Vec::new(),
      entries: Vec::new(),
    }];
    for entry in entries {
      let (dir, name) = match entry.path.iter().rposition(|byte| *byte == SLASH) {
        Some(at) => (
          entry.path.get(..at).unwrap_or_default(),
          entry.path.get(at.saturating_add(1)..).unwrap_or_default(),
        ),
        None => (EMPTY, entry.path),
      };
      while stack.last().is_some_and(|open| !holds(&open.path, dir)) {
        self.close(&mut stack, admission)?;
      }
      open_down_to(&mut stack, dir, admission)?;
      admission.charge_items::<TreeEntry>(1)?;
      admission.charge(to_u64(name.len()).saturating_add(to_u64(entry.path.len())))?;
      if let (Some(open), Some(id)) = (stack.last_mut(), ObjectKey::new(&entry.node)) {
        open.entries.push(TreeEntry {
          name: name.to_vec(),
          kind: kind_of(entry.flag),
          id,
        });
      }
      self.paths.entry(entry.node).or_insert_with(|| entry.path.to_vec());
    }
    while stack.len() > 1 {
      self.close(&mut stack, admission)?;
    }
    let root = stack.pop().map(|open| open.entries).unwrap_or_default();
    self.keep(root, admission)
  }

  /// Closes the innermost open directory: kept under its name, and entered in its parent.
  fn close(&mut self, stack: &mut Vec<Open>, admission: &mut Admission) -> Result<(), LayoutRefusal> {
    let Some(open) = stack.pop() else {
      return Ok(());
    };
    let name = open
      .path
      .rsplit(|byte| *byte == SLASH)
      .next()
      .unwrap_or_default()
      .to_vec();
    let id = self.keep(open.entries, admission)?;
    if let Some(parent) = stack.last_mut() {
      parent.entries.push(TreeEntry {
        name,
        kind: EntryKind::Directory,
        id,
      });
    }
    Ok(())
  }

  /// Keeps a directory's entries under their name, charged first.
  fn keep(&mut self, entries: Vec<TreeEntry>, admission: &mut Admission) -> Result<ObjectKey, LayoutRefusal> {
    admission.charge_items::<(ObjectKey, Vec<TreeEntry>)>(1)?;
    let id = directory_id(&entries).ok_or(LayoutRefusal::Unnamed)?;
    self.dirs.entry(id).or_insert(entries);
    Ok(id)
  }

  /// The text of the file revision `node`: the push's or the repository's, by the path a laid-out manifest named it at.
  fn file_text(&self, node: &Node) -> Result<Cow<'v, [u8]>, Unavailable> {
    let path = self.paths.get(node).ok_or(Unavailable::Missing)?;
    match self.verified.file(path, node) {
      Some(arrived) => Ok(Cow::Borrowed(arrived.text.as_slice())),
      None => self.held.file_text(path, node).ok_or(Unavailable::Missing),
    }
  }

  /// The bytes of the file revision `id`: its text without its metadata block.
  fn bytes(&self, id: ObjectKey) -> Result<Cow<'v, [u8]>, Unavailable> {
    let node = Node::try_from(id.as_bytes()).map_err(|_| Unavailable::NotABlob)?;
    match self.file_text(&node)? {
      Cow::Borrowed(text) => {
        let split = filerev::split(text).map_err(|_| Unavailable::Malformed)?;
        Ok(Cow::Borrowed(split.bytes))
      }
      Cow::Owned(text) => {
        let split = filerev::split(&text).map_err(|_| Unavailable::Malformed)?;
        Ok(Cow::Owned(split.bytes.to_vec()))
      }
    }
  }
}

impl<H: Held + ?Sized> PushObjects for Trees<'_, H> {
  fn tree(&self, id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable> {
    self.dirs.get(&id).cloned().ok_or(Unavailable::Missing)
  }

  fn blob_len(&self, id: ObjectKey) -> Result<u64, Unavailable> {
    Ok(to_u64(self.bytes(id)?.len()))
  }

  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable> {
    self.bytes(id)
  }
}

/// Whether the directory `open` holds the directory `dir`: it is `dir`, or the root, or one of `dir`'s ancestors.
fn holds(open: &[u8], dir: &[u8]) -> bool {
  open.is_empty()
    || open == dir
    || (dir.starts_with(open) && dir.get(open.len()) == Some(&SLASH))
}

/// Opens the directories between the innermost open one and `dir`, outermost first.
fn open_down_to(stack: &mut Vec<Open>, dir: &[u8], admission: &mut Admission) -> Result<(), PastAdmission> {
  let top = stack.last().map(|open| open.path.clone()).unwrap_or_default();
  if top.as_slice() == dir {
    return Ok(());
  }
  let rest = if top.is_empty() {
    dir
  } else {
    dir.get(top.len().saturating_add(1)..).unwrap_or_default()
  };
  let mut path = top;
  for component in rest.split(|byte| *byte == SLASH) {
    if !path.is_empty() {
      path.push(SLASH);
    }
    path.extend_from_slice(component);
    admission.charge_items::<Open>(1)?;
    admission.charge(to_u64(path.len()))?;
    stack.push(Open {
      path: path.clone(),
      entries: Vec::new(),
    });
  }
  Ok(())
}

/// What a manifest entry's flag makes of it in a tree.
fn kind_of(flag: Flag) -> EntryKind {
  match flag {
    Flag::Regular => EntryKind::File,
    Flag::Executable => EntryKind::Executable,
    Flag::Link => EntryKind::Symlink,
  }
}

/// A laid-out directory's name (module docs).
fn directory_id(entries: &[TreeEntry]) -> Option<ObjectKey> {
  let mut hasher = blake3::Hasher::new_derive_key(DIRECTORY_CONTEXT);
  for entry in entries {
    hasher.update(&to_u64(entry.name.len()).to_le_bytes());
    hasher.update(&entry.name);
    let kind = KIND_BYTES
      .iter()
      .find(|(kind, _)| *kind == entry.kind)
      .map_or(u8::MAX, |(_, byte)| *byte);
    hasher.update(&[kind]);
    let id = entry.id.as_bytes();
    hasher.update(&to_u64(id.len()).to_le_bytes());
    hasher.update(id);
  }
  let digest: [u8; blake3::OUT_LEN] = *hasher.finalize().as_bytes();
  ObjectKey::new(&digest)
}

/// One move a push makes of a line or a bookmark: the push's name (two submits of one push carry the same), the version
/// it is based on and that version's head changeset (none for a green with nothing in it yet), and the new head.
#[derive(Clone, Copy, Debug)]
pub struct Move<'a> {
  /// The push's name.
  pub name: [u8; 32],
  /// The version the move is based on.
  pub base_version: u64,
  /// That version's head changeset.
  pub base: Option<&'a Node>,
  /// The new head changeset.
  pub head: &'a Node,
}

/// The work a push makes of one line's or bookmark's move (§4.17.1 "A push is a work"): from the move's base version to
/// its head changeset, whose node is the head the work sets; both trees laid out from their manifests, the push's
/// revisions over the repository's, and compared by A-618's builder within `room`.
pub fn work(
  moved: &Move<'_>,
  green: &Green,
  revisions: (&Verified, &(impl Held + ?Sized)),
  room: u64,
  admission: &mut Admission,
) -> Result<Built, WorkRefused> {
  let (verified, held) = revisions;
  let mut trees = Trees::new(verified, held);
  let base_tree = match moved.base.filter(|base| **base != NULL_NODE) {
    Some(base) => Some(
      trees
        .lay_out_changeset(base, admission)
        .map_err(WorkRefused::Layout)?,
    ),
    None => None,
  };
  let tree = trees
    .lay_out_changeset(moved.head, admission)
    .map_err(WorkRefused::Layout)?;
  let push = Push {
    name: moved.name,
    base_version: moved.base_version,
    base_tree,
    tree,
    head: Some(moved.head.as_slice()),
  };
  push::build(&push, green, &trees, room).map_err(WorkRefused::Push)
}
