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
use slates_merge::push::{
  self, Built, EntryKind, ObjectKey, Push, PushObjects, PushRefused, TreeEntry, Unavailable,
};

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
  pub fn lay_out(
    &mut self,
    node: &Node,
    admission: &mut Admission,
  ) -> Result<ObjectKey, LayoutRefusal> {
    let verified = self.verified;
    let held = self.held;
    let text =
      manifest_text(verified, held, node).ok_or(LayoutRefusal::MissingManifest { node: *node })?;
    let entries =
      manifest::decode(&text).map_err(|fault| LayoutRefusal::Manifest { node: *node, fault })?;
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
      self
        .paths
        .entry(entry.node)
        .or_insert_with(|| entry.path.to_vec());
    }
    while stack.len() > 1 {
      self.close(&mut stack, admission)?;
    }
    let root = stack.pop().map(|open| open.entries).unwrap_or_default();
    self.keep(root, admission)
  }

  /// Closes the innermost open directory: kept under its name, and entered in its parent.
  fn close(
    &mut self,
    stack: &mut Vec<Open>,
    admission: &mut Admission,
  ) -> Result<(), LayoutRefusal> {
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
  fn keep(
    &mut self,
    entries: Vec<TreeEntry>,
    admission: &mut Admission,
  ) -> Result<ObjectKey, LayoutRefusal> {
    admission.charge_items::<(ObjectKey, Vec<TreeEntry>)>(1)?;
    let id = directory_id(&entries).ok_or(LayoutRefusal::Unnamed)?;
    self.dirs.entry(id).or_insert(entries);
    Ok(id)
  }

  /// The text of the file revision `node`: the push's or the repository's, by the path a laid-out manifest named it at.
  /// A revision the repository holds whose text is not at hand (let go to the node's changeset store, A-767 C1) is
  /// [`Unavailable::NotYet`]: the build waits for it while its owner reads it from the store.
  fn file_text(&self, node: &Node) -> Result<Cow<'v, [u8]>, Unavailable> {
    let path = self.paths.get(node).ok_or(Unavailable::Missing)?;
    match self.verified.file(path, node) {
      Some(arrived) => Ok(Cow::Borrowed(arrived.text.as_slice())),
      None => self.held.file_text(path, node).ok_or_else(|| {
        if self.held.file_names(path, node).is_some() {
          Unavailable::NotYet
        } else {
          Unavailable::Missing
        }
      }),
    }
  }

  /// The length of the file revision `id`'s bytes: the push's from its text, the repository's from its row, never its
  /// text read (a walk's unit asks it, and cannot wait).
  fn bytes_len(&self, id: ObjectKey) -> Result<u64, Unavailable> {
    let node = Node::try_from(id.as_bytes()).map_err(|_| Unavailable::NotABlob)?;
    let path = self.paths.get(&node).ok_or(Unavailable::Missing)?;
    match self.verified.file(path, &node) {
      Some(arrived) => {
        let split = filerev::split(&arrived.text).map_err(|_| Unavailable::Malformed)?;
        Ok(to_u64(split.bytes.len()))
      }
      None => self
        .held
        .file_bytes_len(path, &node)
        .ok_or(Unavailable::Missing),
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
    self.bytes_len(id)
  }

  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable> {
    self.bytes(id)
  }
}

/// Whether the directory `open` holds the directory `dir`: it is `dir`, or the root, or one of `dir`'s ancestors.
fn holds(open: &[u8], dir: &[u8]) -> bool {
  open.is_empty() || open == dir || (dir.starts_with(open) && dir.get(open.len()) == Some(&SLASH))
}

/// Opens the directories between the innermost open one and `dir`, outermost first.
fn open_down_to(
  stack: &mut Vec<Open>,
  dir: &[u8],
  admission: &mut Admission,
) -> Result<(), PastAdmission> {
  let top = stack
    .last()
    .map(|open| open.path.clone())
    .unwrap_or_default();
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

/// A version's trees laid out, owned (A-767 C1, the push spin w-balance found): what a work's build keeps between its
/// turns, so its manifests are laid out once, not a turn at a time.
pub struct Layout {
  /// Every directory laid out, by its name.
  dirs: BTreeMap<ObjectKey, Vec<TreeEntry>>,
  /// Each file node a laid-out manifest names, with the path naming it.
  paths: BTreeMap<Node, Vec<u8>>,
}

/// A push's work built a unit a turn (A-767 C1, the push spin: a 64 MiB file's work built in one step spun its shard
/// past the watchdog; main's ruling of 2026-11-21 13:26, (1)): both trees laid out once ([`begin_work`]), then A-618's
/// sliced builder advanced over them a turn at a time, the push's revisions and the repository's lent each turn
/// ([`WorkBuild::advance`]). The work is the push's own until its landing takes it: nothing else reads it.
pub struct WorkBuild {
  /// The trees laid out.
  layout: Layout,
  /// The builder.
  building: push::Building,
  /// The repository's file revision the build waits for, read from the node's changeset store between turns
  /// ([`WorkBuild::ask_waited`]).
  asked: Option<TextAsk>,
}

/// A file revision the repository holds whose text the build waits for (A-767 C1: a text an import let go): its rows
/// asked of the node's changeset store, then its whole text, within the room its turn had.
struct TextAsk {
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

/// Begins building the work of `moved` (as [`work`] builds it whole): its base's and its head's trees laid out within
/// `admission`, and the builder made within `room`, its turns under `pace`.
pub fn begin_work(
  moved: &Move<'_>,
  revisions: (&Verified, &(impl Held + ?Sized)),
  (room, pace): (u64, slates_merge::pace::Pace),
  admission: &mut Admission,
) -> Result<WorkBuild, WorkRefused> {
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
  let pushed = Push {
    name: moved.name,
    base_version: moved.base_version,
    base_tree,
    tree,
    head: Some(moved.head.as_slice()),
  };
  let building = push::Building::new(&pushed, room, pace);
  Ok(WorkBuild {
    layout: Layout {
      dirs: trees.dirs,
      paths: trees.paths,
    },
    building,
    asked: None,
  })
}

impl WorkBuild {
  /// Begins asking the node's changeset store for the file revision the last turn waited for ([`push::Building::waiting`],
  /// A-767 C1), its text within `most` bytes: whether one was waited for. Its owner answers the ask
  /// ([`crate::render::StoreWants`]), then takes the text ([`WorkBuild::take_given`]) into its content store before the next
  /// turn.
  pub fn ask_waited(&mut self, most: u64) -> bool {
    let Some(node) = self
      .building
      .waiting()
      .and_then(|id| Node::try_from(id.as_bytes()).ok())
    else {
      return false;
    };
    let Some(path) = self.layout.paths.get(&node).cloned() else {
      return false;
    };
    self.asked = Some(TextAsk {
      want: Some(crate::render::Want::Rows {
        logs: vec![(path.clone(), vec![node])],
      }),
      path,
      node,
      most,
      text: None,
    });
    true
  }

  /// The text the store gave for the revision asked ([`WorkBuild::ask_waited`]), taken with its path and node; none when
  /// the store kept no such revision or gave no text.
  pub fn take_given(&mut self) -> Option<(Vec<u8>, Node, Vec<u8>)> {
    let asked = self.asked.take()?;
    let text = asked.text?;
    Some((asked.path, asked.node, text))
  }

  /// The pace its turns run under: its owner begins each turn on it.
  pub fn pace(&self) -> &slates_merge::pace::Pace {
    self.building.pace()
  }

  /// One turn over `green`, its objects the trees laid out over `revisions` (the push's and the repository's, lent for
  /// the turn): the work or the refusal once built; none while units remain.
  pub fn advance<H: Held + ?Sized>(
    &mut self,
    green: &Green,
    (verified, held): (&Verified, &H),
  ) -> Option<Result<Built, WorkRefused>> {
    let trees = Trees {
      verified,
      held,
      dirs: core::mem::take(&mut self.layout.dirs),
      paths: core::mem::take(&mut self.layout.paths),
    };
    let advanced = self.building.advance(green, &trees);
    self.layout = Layout {
      dirs: trees.dirs,
      paths: trees.paths,
    };
    advanced.map(|built| built.map_err(WorkRefused::Push))
  }
}

impl crate::render::StoreWants for WorkBuild {
  fn wants(&self) -> Option<&crate::render::Want> {
    self.asked.as_ref().and_then(|asked| asked.want.as_ref())
  }

  fn give_rows(&mut self, rows: Vec<Vec<Option<crate::render::StoredRevision>>>) {
    let Some(asked) = self.asked.as_mut() else {
      return;
    };
    asked.want = rows
      .into_iter()
      .next()
      .and_then(|log| log.into_iter().next())
      .flatten()
      .map(|row| crate::render::Want::Texts {
        texts: vec![row.text],
        most: asked.most,
      });
  }

  fn give_chunks(&mut self, _chunks: Vec<Vec<u8>>) {
    if let Some(asked) = self.asked.as_mut() {
      asked.want = None;
    }
  }

  fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    let Some(asked) = self.asked.as_mut() else {
      return;
    };
    asked.want = None;
    asked.text = texts.into_iter().next();
  }

  fn reads_bases(&self) -> bool {
    true
  }
}
