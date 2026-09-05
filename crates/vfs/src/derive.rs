//! The deriver (§4.16 "Composition at seal", D-27, Phase 1 task 14): from the declared
//! operations a work volume journaled since its base snapshot, the ops document of the
//! increment: for every path the work touched, what the post-state holds there relative to
//! the base, as a canonical byte encoding whose BLAKE3 hash is the document's identity.
//!
//! Nothing here compares file contents. Content deltas come from the interval algebra
//! ([`crate::algebra`]) over the journaled ranges; names come from the two trees (the base
//! snapshot's and the head's) at the paths the journal names, and from a file's home when a
//! content operation named only its inode. The document is a state delta over paths: files
//! with their base reference and hunks, symlinks with their targets, directories created and
//! removed, entries removed. It is sorted by path, so the same journal gives the same bytes on
//! every platform (AC-1.15); a whole-file rewrite by any route is one hunk (T-1.19).
//!
//! Cost is proportional to the touched paths, not the tree: the journal is read once, each
//! touched path is resolved in both trees, and only an inode that has had more than one link
//! (or whose home no longer names it) costs a walk.
//!
//! An applier reads the document as a set with one order: renamed directories are detached
//! from their base paths, then removals apply, then the detached subtrees attach at their new
//! paths, then directories are created, then symlinks, then files; a file's base reference
//! names its base path before any of that, so it is looked up in the base as it was.

use std::collections::{BTreeMap, BTreeSet};

use crate::algebra::{ContentMap, ContentOp, Hunk};
use crate::dir::Child;
use crate::error::VfsError;
use crate::ids::{InodeNo, SnapshotId};
use crate::journal::Op;
use crate::volume::{Store, Volume};

/// Format: the document's magic, version and the field widths of its encoding.
const MAGIC: &[u8; 4] = b"SLOP";
/// Format: the encoding version; bumped with any change to the layout below.
const VERSION: u16 = 1;

/// The base file whose bytes a delta's surviving runs come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseRef {
  /// The base path of that file.
  pub path: Box<str>,
  /// Its length in the base.
  pub len: u64,
}

/// A file of the post-state, relative to the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDelta {
  /// The path in the post-state.
  pub path: Box<str>,
  /// The base file its surviving bytes come from (none for a file with no base bytes).
  pub base: Option<BaseRef>,
  /// The post-state length.
  pub post_len: u64,
  /// The net edit.
  pub hunks: Vec<Hunk>,
}

/// A symlink of the post-state that differs from the base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymlinkDelta {
  /// The path.
  pub path: Box<str>,
  /// The target.
  pub target: Box<str>,
}

/// The ops document of an increment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpsDocument {
  /// Directories renamed, with everything beneath them, as `(base path, post-state path)`;
  /// applied first, so a path beneath a renamed directory is named by its post-state path
  /// everywhere else in the document (§4.15's one rename per directory; §4.16's remapping).
  pub dirs_renamed: Vec<(Box<str>, Box<str>)>,
  /// Directories the post-state has and the base did not.
  pub dirs_created: Vec<Box<str>>,
  /// Directories the base had and the post-state does not.
  pub dirs_removed: Vec<Box<str>>,
  /// Files and symlinks the base had and the post-state does not (or has as a directory).
  pub removed: Vec<Box<str>>,
  /// Symlinks of the post-state that are new or changed.
  pub symlinks: Vec<SymlinkDelta>,
  /// Files of the post-state that are new, moved or changed.
  pub files: Vec<FileDelta>,
}

impl OpsDocument {
  /// The canonical encoding: little-endian, length-prefixed strings, sections in a fixed order.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    put_count(&mut out, self.dirs_renamed.len());
    for (from, to) in &self.dirs_renamed {
      put_str(&mut out, from);
      put_str(&mut out, to);
    }
    put_paths(&mut out, &self.dirs_created);
    put_paths(&mut out, &self.dirs_removed);
    put_paths(&mut out, &self.removed);
    put_count(&mut out, self.symlinks.len());
    for s in &self.symlinks {
      put_str(&mut out, &s.path);
      put_str(&mut out, &s.target);
    }
    put_count(&mut out, self.files.len());
    for f in &self.files {
      put_str(&mut out, &f.path);
      match &f.base {
        Some(b) => {
          out.push(1);
          put_str(&mut out, &b.path);
          out.extend_from_slice(&b.len.to_le_bytes());
        }
        None => out.push(0),
      }
      out.extend_from_slice(&f.post_len.to_le_bytes());
      put_count(&mut out, f.hunks.len());
      for h in &f.hunks {
        out.extend_from_slice(&h.base_at.to_le_bytes());
        out.extend_from_slice(&h.base_len.to_le_bytes());
        out.extend_from_slice(&h.post_at.to_le_bytes());
        out.extend_from_slice(&h.new_len.to_le_bytes());
      }
    }
    out
  }

  /// The document's identity: BLAKE3 of its encoding (D-17).
  pub fn identity(&self) -> [u8; 32] {
    *blake3::hash(&self.encode()).as_bytes()
  }

  /// Whether the increment changes nothing.
  pub fn is_empty(&self) -> bool {
    self.dirs_renamed.is_empty()
      && self.dirs_created.is_empty()
      && self.dirs_removed.is_empty()
      && self.removed.is_empty()
      && self.symlinks.is_empty()
      && self.files.is_empty()
  }
}

fn put_count(out: &mut Vec<u8>, n: usize) {
  out.extend_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
  out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_le_bytes());
  out.extend_from_slice(s.as_bytes());
}

fn put_paths(out: &mut Vec<u8>, paths: &[Box<str>]) {
  put_count(out, paths.len());
  for p in paths {
    put_str(out, p);
  }
}

/// What a path holds on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Entry {
  None,
  Dir(InodeNo),
  File(InodeNo),
  Symlink(InodeNo),
}

fn classify(located: Result<crate::volume::Located, VfsError>) -> Entry {
  match located {
    Ok(l) => match l.child {
      Child::Dir(_) => Entry::Dir(l.inode),
      Child::File(no) => Entry::File(no),
      Child::Symlink(no) => Entry::Symlink(no),
      Child::Whiteout => Entry::None,
    },
    Err(_) => Entry::None,
  }
}

/// The content operation a journal record declares, if it is one.
fn content_op(op: &Op) -> Option<ContentOp> {
  Some(match *op {
    Op::Overwrite { at, len } => ContentOp::Overwrite { at, len },
    Op::Extend { at, len } => ContentOp::Extend { at, len },
    Op::Truncate { len } => ContentOp::Truncate { len },
    Op::Insert { at, len } => ContentOp::Insert { at, len },
    Op::Delete { at, len } => ContentOp::Delete { at, len },
    _ => return None,
  })
}

/// The deriver's working state for one increment.
struct Deriver<'a> {
  vol: &'a Volume,
  store: &'a Store,
  base: SnapshotId,
  /// Content maps of the inodes the work wrote, relative to their base bytes.
  maps: BTreeMap<InodeNo, ContentMap>,
  /// Every path the work touched, on either side.
  touched: BTreeSet<String>,
}

impl Deriver<'_> {
  /// Folds the journal: content operations into maps, paths into the touched set.
  fn fold(&mut self, records: &[crate::journal::OpRecord]) {
    for r in records {
      if let (Some(op), Some(no)) = (content_op(&r.op), r.inode) {
        let base_len = self
          .vol
          .stat_in(self.store, self.base, no)
          .ok()
          .map(|a| a.size);
        self
          .maps
          .entry(no)
          .or_insert_with(|| base_len.map_or_else(ContentMap::created, ContentMap::identity))
          .apply(op);
      }
      if !r.path.is_empty() {
        self.touched.insert(r.path.to_string());
      }
      if let Op::Rename { from } | Op::Redirect { from } = &r.op {
        self.touched.insert(from.to_string());
      }
    }
    // A content-written inode is touched at every path it has, on both sides.
    let written: Vec<InodeNo> = self.maps.keys().copied().collect();
    for no in written {
      for p in self.head_paths(no) {
        self.touched.insert(p);
      }
      for p in self.base_paths(no) {
        self.touched.insert(p);
      }
    }
  }

  fn head_paths(&self, no: InodeNo) -> Vec<String> {
    let multi =
      self.vol.stat(self.store, no).is_ok() && self.vol.inode_multi(self.store, no).unwrap_or(true);
    if !multi && let Some(p) = self.vol.path_of_inode(self.store, no) {
      return vec![p];
    }
    self
      .vol
      .paths_of_inode_walk(self.store, self.vol.root(), no)
  }

  fn base_paths(&self, no: InodeNo) -> Vec<String> {
    let Ok((_, root)) = self.vol.snapshot_info(self.base) else {
      return Vec::new();
    };
    let multi = self
      .vol
      .inode_multi_in(self.store, self.base, no)
      .unwrap_or(true);
    if !multi && let Some(p) = self.vol.path_of_inode_in(self.store, self.base, no) {
      return vec![p];
    }
    self.vol.paths_of_inode_walk(self.store, root, no)
  }

  /// The base file whose bytes an inode's surviving runs come from, and its length.
  fn base_ref_of(&self, no: InodeNo) -> Option<BaseRef> {
    let len = self.vol.stat_in(self.store, self.base, no).ok()?.size;
    let path = self.base_paths(no).into_iter().next()?;
    Some(BaseRef {
      path: path.into(),
      len,
    })
  }

  /// One touched path into the document.
  fn classify_path(&self, path: &str, doc: &mut OpsDocument) -> Result<(), VfsError> {
    let base = classify(self.vol.resolve_in(self.store, self.base, path));
    let head = classify(self.vol.resolve(self.store, path));
    match (base, head) {
      (Entry::None, Entry::None) => {}
      (Entry::Dir(b), Entry::Dir(h)) if b == h => {}
      (_, Entry::Dir(no)) => {
        // A directory the base held at another path moved here with its subtree.
        match self.vol.path_of_dir_in(self.store, self.base, no) {
          Some(from) if from != path => {
            self.removed_side(base, path, doc);
            doc.dirs_renamed.push((from.into(), path.into()));
          }
          _ => {
            self.removed_side(base, path, doc);
            doc.dirs_created.push(path.into());
          }
        }
      }
      (Entry::Dir(_), Entry::None) => doc.dirs_removed.push(path.into()),
      (Entry::File(_) | Entry::Symlink(_), Entry::None) => doc.removed.push(path.into()),
      (_, Entry::Symlink(no)) => {
        let target = self.vol.readlink(self.store, no)?;
        let same = matches!(base, Entry::Symlink(b) if self.vol.readlink(self.store, b).as_deref() == Ok(&target));
        if !same {
          match base {
            Entry::Dir(_) => doc.dirs_removed.push(path.into()),
            // A file gave way to a symlink; a symlink with another target is replaced by
            // the new one below.
            Entry::File(_) => doc.removed.push(path.into()),
            Entry::Symlink(_) | Entry::None => {}
          }
          doc.symlinks.push(SymlinkDelta {
            path: path.into(),
            target,
          });
        }
      }
      (_, Entry::File(no)) => {
        match base {
          Entry::Dir(_) => doc.dirs_removed.push(path.into()),
          // A symlink gave way to a file; a file is replaced through the delta's base.
          Entry::Symlink(_) => doc.removed.push(path.into()),
          Entry::File(_) | Entry::None => {}
        }
        self.file_delta(path, base, no, doc)?;
      }
    }
    Ok(())
  }

  fn removed_side(&self, base: Entry, path: &str, doc: &mut OpsDocument) {
    match base {
      Entry::File(_) | Entry::Symlink(_) => doc.removed.push(path.into()),
      Entry::Dir(_) => doc.dirs_removed.push(path.into()),
      Entry::None => {}
    }
  }

  /// A file of the post-state at `path`: unchanged files are left out; a renamed file names
  /// its base path; a replaced path names the replaced file's length so a rewrite by any route
  /// is one hunk.
  fn file_delta(
    &self,
    path: &str,
    base: Entry,
    no: InodeNo,
    doc: &mut OpsDocument,
  ) -> Result<(), VfsError> {
    let post_len = self.vol.stat(self.store, no)?.size;
    let own_base = self.base_ref_of(no);
    let map = self.maps.get(&no).cloned();
    let same_place = matches!(base, Entry::File(b) if b == no);
    if same_place && map.is_none() {
      return Ok(());
    }
    let (base_ref, map) = match (own_base, map) {
      // The inode's own base bytes survive (possibly at another path).
      (Some(b), Some(m)) => (Some(b), m),
      (Some(b), None) => {
        let m = ContentMap::identity(b.len);
        (Some(b), m)
      }
      // No base bytes of its own: the path's base file, if any, is replaced whole.
      (None, m) => {
        let replaced = match base {
          Entry::File(b) => Some(BaseRef {
            path: path.into(),
            len: self.vol.stat_in(self.store, self.base, b)?.size,
          }),
          _ => None,
        };
        (replaced, m.unwrap_or_else(ContentMap::created))
      }
    };
    let base_len = base_ref.as_ref().map_or(0, |b| b.len);
    doc.files.push(FileDelta {
      path: path.into(),
      base: base_ref,
      post_len,
      hunks: map.hunks(base_len),
    });
    Ok(())
  }
}

/// Derives the ops document of the work since `base`.
pub fn derive(vol: &Volume, store: &Store, base: SnapshotId) -> Result<OpsDocument, VfsError> {
  let records = vol.records_since(base)?;
  let mut d = Deriver {
    vol,
    store,
    base,
    maps: BTreeMap::new(),
    touched: BTreeSet::new(),
  };
  d.fold(&records);
  let mut doc = OpsDocument::default();
  let mut classified: BTreeSet<String> = BTreeSet::new();
  for path in d.touched.clone() {
    d.classify_once(&path, &mut doc, &mut classified)?;
  }
  // Entries beneath a created or renamed directory were journaled under old names or not at
  // all (a directory created empty and filled later, then moved); the head's subtree says
  // what is there now. What moved unchanged with a renamed parent is skipped: the rename
  // carries it.
  let mut queue: Vec<(String, Option<String>)> = doc
    .dirs_created
    .iter()
    .map(|p| (p.to_string(), None))
    .chain(
      doc
        .dirs_renamed
        .iter()
        .map(|(from, to)| (to.to_string(), Some(from.to_string()))),
    )
    .collect();
  while let Some((head_dir, base_dir)) = queue.pop() {
    d.walk_subtree(
      &head_dir,
      base_dir.as_deref(),
      &mut doc,
      &mut classified,
      &mut queue,
    )?;
  }
  doc.dirs_renamed.sort();
  let sources: Vec<Box<str>> = doc
    .dirs_renamed
    .iter()
    .map(|(from, _)| from.clone())
    .collect();
  doc.dirs_removed.retain(|p| !sources.contains(p));
  doc.dirs_created.sort();
  doc.dirs_created.dedup();
  doc.dirs_removed.sort();
  doc.dirs_removed.dedup();
  doc.removed.sort();
  doc.removed.dedup();
  doc.symlinks.sort_by(|a, b| a.path.cmp(&b.path));
  doc.files.sort_by(|a, b| a.path.cmp(&b.path));
  Ok(doc)
}

impl Deriver<'_> {
  /// Classifies a path once.
  fn classify_once(
    &self,
    path: &str,
    doc: &mut OpsDocument,
    classified: &mut BTreeSet<String>,
  ) -> Result<(), VfsError> {
    if classified.insert(path.to_owned()) {
      self.classify_path(path, doc)?;
    }
    Ok(())
  }

  /// One level of a head subtree: every entry is classified unless it moved unchanged with a
  /// renamed parent (`base_dir` is the parent's base path then); subdirectories queue up.
  fn walk_subtree(
    &self,
    head_dir: &str,
    base_dir: Option<&str>,
    doc: &mut OpsDocument,
    classified: &mut BTreeSet<String>,
    queue: &mut Vec<(String, Option<String>)>,
  ) -> Result<(), VfsError> {
    let Ok(located) = self.vol.resolve(self.store, head_dir) else {
      return Ok(());
    };
    let Child::Dir(dir) = located.child else {
      return Ok(());
    };
    let rows: Vec<(String, Child, InodeNo)> = self
      .vol
      .readdir(self.store, dir)?
      .iter()
      .map(|r| {
        let child = match r.kind {
          crate::inode::Kind::Dir => Child::Whiteout,
          crate::inode::Kind::File => Child::File(r.inode),
          crate::inode::Kind::Symlink => Child::Symlink(r.inode),
        };
        (r.name.to_owned(), child, r.inode)
      })
      .collect();
    for (name, child, no) in rows {
      let head_path = format!("{}/{name}", head_dir.trim_end_matches('/'));
      let base_path = base_dir.map(|b| format!("{}/{name}", b.trim_end_matches('/')));
      match child {
        Child::Whiteout => {
          // A subdirectory: moved with its parent when its base path is the parent's
          // counterpart, else created or renamed on its own.
          let own = self.vol.path_of_dir_in(self.store, self.base, no);
          if own.is_some() && own == base_path {
            queue.push((head_path, base_path));
          } else {
            self.classify_once(&head_path, doc, classified)?;
            let mapping = own.filter(|p| p != &head_path);
            queue.push((head_path, mapping));
          }
        }
        Child::File(_) | Child::Symlink(_) => {
          let unchanged = base_path.as_deref().is_some_and(|bp| {
            !self.maps.contains_key(&no)
              && self
                .vol
                .resolve_in(self.store, self.base, bp)
                .is_ok_and(|l| l.inode == no)
          });
          if !unchanged {
            self.classify_once(&head_path, doc, classified)?;
          }
        }
        Child::Dir(_) => {}
      }
    }
    Ok(())
  }
}
