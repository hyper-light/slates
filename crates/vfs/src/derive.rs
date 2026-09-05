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
    self.dirs_created.is_empty()
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
  Dir,
  File(InodeNo),
  Symlink(InodeNo),
}

fn classify(located: Result<crate::volume::Located, VfsError>) -> Entry {
  match located {
    Ok(l) => match l.child {
      Child::Dir(_) => Entry::Dir,
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
      (Entry::None, Entry::None) | (Entry::Dir, Entry::Dir) => {}
      (_, Entry::Dir) => {
        self.removed_side(base, path, doc);
        doc.dirs_created.push(path.into());
      }
      (Entry::Dir, Entry::None) => doc.dirs_removed.push(path.into()),
      (Entry::File(_) | Entry::Symlink(_), Entry::None) => doc.removed.push(path.into()),
      (_, Entry::Symlink(no)) => {
        let target = self.vol.readlink(self.store, no)?;
        let same = matches!(base, Entry::Symlink(b) if self.vol.readlink(self.store, b).as_deref() == Ok(&target));
        if !same {
          match base {
            Entry::Dir => doc.dirs_removed.push(path.into()),
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
          Entry::Dir => doc.dirs_removed.push(path.into()),
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
      Entry::Dir | Entry::None => {}
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
  for path in &d.touched {
    d.classify_path(path, &mut doc)?;
  }
  doc.dirs_created.sort();
  doc.dirs_removed.sort();
  doc.removed.sort();
  doc.symlinks.sort_by(|a, b| a.path.cmp(&b.path));
  doc.files.sort_by(|a, b| a.path.cmp(&b.path));
  Ok(doc)
}
