//! Driving a volume by paths: the step applier the differential and deriver suites share
//! (they pick files by sorted path, so both sides of a comparison address the same file), and
//! the path-keyed views of a head and of a snapshot.

// Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
#![allow(clippy::indexing_slicing)]
use std::collections::BTreeMap;

use slates_mem::Handle;
use slates_vfs::dir::{Child, DirNode};
use slates_vfs::error::VfsError;
use slates_vfs::ids::{InodeNo, SnapshotId};
use slates_vfs::inode::Kind;
use slates_vfs::volume::{Store, Volume};

use super::steps::{Step, XattrMode};
use slates_vfs::xattr::XattrSet;

/// The directory at `path`, distinguishing a missing component (`ENOENT`) from one that is not
/// a directory (`ENOTDIR`), as the kernel does.
pub(crate) fn dir_at(
  vol: &Volume,
  store: &Store,
  path: &[String],
) -> Result<Handle<DirNode>, VfsError> {
  let mut dir = vol.root();
  for p in path {
    match vol.lookup(store, dir, p)?.child {
      Child::Dir(h) => dir = h,
      _ => return Err(VfsError::NotDirectory),
    }
  }
  Ok(dir)
}

/// The regular file at an absolute path.
pub(crate) fn file_at(vol: &Volume, store: &Store, path: &str) -> Result<InodeNo, VfsError> {
  match vol.resolve(store, path)?.child {
    Child::File(no) => Ok(no),
    Child::Dir(_) => Err(VfsError::IsDirectory),
    _ => Err(VfsError::Invalid),
  }
}

/// The `pick`-th regular file by sorted path, wrapping; none when there are no files.
pub(crate) fn pick_file(files: &[String], pick: u8) -> Option<&str> {
  if files.is_empty() {
    None
  } else {
    Some(&files[usize::from(pick) % files.len()])
  }
}

/// An operation on a resolved directory.
type DirOp<'a> =
  &'a mut dyn FnMut(&mut Volume, &mut Store, Handle<DirNode>) -> Result<(), VfsError>;

/// Applies one step to the volume; `files` are the head's regular files by sorted path for
/// the picks. `None` when a pick has nothing to choose from.
pub(crate) fn apply_volume(
  step: &Step,
  vol: &mut Volume,
  store: &mut Store,
  files: &[String],
) -> Option<Result<(), VfsError>> {
  let dir_then = |vol: &mut Volume, store: &mut Store, p: &[String], op: DirOp| {
    let d = dir_at(vol, store, p)?;
    op(vol, store, d)
  };
  Some(match step {
    Step::Create(p, n) => dir_then(vol, store, p, &mut |v, s, d| {
      v.create_file(s, d, n, 0o644).map(drop)
    }),
    Step::Mkdir(p, n) => dir_then(vol, store, p, &mut |v, s, d| {
      v.mkdir(s, d, n, 0o755).map(drop)
    }),
    Step::Symlink(p, n) => dir_then(vol, store, p, &mut |v, s, d| {
      v.symlink(s, d, n, ".anchor").map(drop)
    }),
    Step::Unlink(p, n) => dir_then(vol, store, p, &mut |v, s, d| v.unlink(s, d, n)),
    Step::Rmdir(p, n) => dir_then(vol, store, p, &mut |v, s, d| v.rmdir(s, d, n)),
    Step::Rename(fp, fnm, tp, tn) => match (dir_at(vol, store, fp), dir_at(vol, store, tp)) {
      (Ok(f), Ok(t)) => vol.rename(store, f, fnm, t, tn),
      (Err(e), _) | (_, Err(e)) => Err(e),
    },
    Step::Link(p, n, pick) => {
      let target = pick_file(files, *pick)?;
      match file_at(vol, store, target) {
        Ok(no) => dir_then(vol, store, p, &mut |v, s, d| v.link(s, d, n, no)),
        Err(e) => Err(e),
      }
    }
    Step::Write(pick, off, bytes) => {
      let target = pick_file(files, *pick)?;
      file_at(vol, store, target)
        .and_then(|no| vol.write(store, no, u64::from(*off), bytes).map(drop))
    }
    Step::Truncate(pick, len) => {
      let target = pick_file(files, *pick)?;
      file_at(vol, store, target).and_then(|no| vol.truncate(store, no, u64::from(*len)))
    }
    Step::Edit(pick, at, del, bytes) => {
      let target = pick_file(files, *pick)?;
      file_at(vol, store, target).and_then(|no| {
        let size = vol.stat(store, no)?.size;
        let at = u64::from(*at) % (size + 1);
        vol.edit(store, no, at, u64::from(*del), bytes)
      })
    }
    Step::Snapshot => vol.snapshot(store).map(drop),
    Step::SetXattr(pick, name, value, mode) => {
      let target = pick_file(files, *pick)?;
      file_at(vol, store, target)
        .and_then(|no| vol.xattr_set(store, no, name.as_bytes(), value, xattr_set(*mode)))
    }
    Step::RemoveXattr(pick, name) => {
      let target = pick_file(files, *pick)?;
      file_at(vol, store, target).and_then(|no| vol.xattr_remove(store, no, name.as_bytes()))
    }
  })
}

/// The volume's set mode for a generated one.
pub(crate) fn xattr_set(mode: XattrMode) -> XattrSet {
  match mode {
    XattrMode::Either => XattrSet::Either,
    XattrMode::Create => XattrSet::Create,
    XattrMode::Replace => XattrSet::Replace,
  }
}

/// A regular file's extended attributes: `(name, value)`, ascending by name.
pub(crate) type Xattrs = Vec<(Vec<u8>, Vec<u8>)>;

/// A tree by paths: every directory with its sorted `(name, kind)` rows, every regular file's
/// bytes and link count, every symlink's target, and every regular file's extended attributes
/// (a file with none is absent).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PathState {
  pub(crate) dirs: Vec<(String, Vec<(String, char)>)>,
  pub(crate) files: BTreeMap<String, (Vec<u8>, u64)>,
  pub(crate) symlinks: BTreeMap<String, String>,
  pub(crate) xattrs: BTreeMap<String, Xattrs>,
}

impl PathState {
  /// The regular files by sorted path.
  pub(crate) fn file_paths(&self) -> Vec<String> {
    self.files.keys().cloned().collect()
  }

  /// The directory paths (the root as the empty string).
  pub(crate) fn dir_paths(&self) -> Vec<String> {
    self.dirs.iter().map(|(p, _)| p.clone()).collect()
  }
}

/// The head's view.
pub(crate) fn head_state(vol: &Volume, store: &Store) -> PathState {
  walk(vol, store, vol.root(), None)
}

/// A snapshot's view.
pub(crate) fn snapshot_state(vol: &Volume, store: &Store, id: SnapshotId) -> PathState {
  let (_, root) = vol.snapshot_info(id).unwrap();
  walk(vol, store, root, Some(id))
}

fn walk(vol: &Volume, store: &Store, root: Handle<DirNode>, snap: Option<SnapshotId>) -> PathState {
  let mut state = PathState::default();
  let mut stack = vec![(String::new(), root)];
  while let Some((prefix, dir)) = stack.pop() {
    let rows = match snap {
      Some(_) => vol.readdir_in(store, dir).unwrap(),
      None => vol.readdir(store, dir).unwrap(),
    };
    let mut names = Vec::new();
    for row in &rows {
      let kind = match row.kind {
        Kind::Dir => 'd',
        Kind::File => 'f',
        Kind::Symlink => 'l',
        Kind::Fifo => 'p',
        Kind::Socket => 's',
      };
      names.push((row.name.to_string(), kind));
      let path = format!("{prefix}/{}", row.name);
      match row.kind {
        Kind::Dir => {
          let located = match snap {
            Some(_) => vol.lookup_in(store, dir, row.name).unwrap(),
            None => vol.lookup(store, dir, row.name).unwrap(),
          };
          if let Child::Dir(h) = located.child {
            stack.push((path, h));
          }
        }
        Kind::File => {
          let xattrs = xattr_rows(vol, store, snap, row.inode);
          if !xattrs.is_empty() {
            state.xattrs.insert(path.clone(), xattrs);
          }
          state
            .files
            .insert(path, file_row(vol, store, snap, row.inode));
        }
        Kind::Fifo | Kind::Socket => {}
        Kind::Symlink => {
          state
            .symlinks
            .insert(path, symlink_row(vol, store, snap, row.inode));
        }
      }
    }
    names.sort();
    state.dirs.push((prefix, names));
  }
  state.dirs.sort();
  state
}

fn file_row(vol: &Volume, store: &Store, snap: Option<SnapshotId>, no: InodeNo) -> (Vec<u8>, u64) {
  let (size, nlink) = match snap {
    Some(id) => {
      let a = vol.stat_in(store, id, no).unwrap();
      (a.size, a.nlink)
    }
    None => {
      let a = vol.stat(store, no).unwrap();
      (a.size, a.nlink)
    }
  };
  let mut buf = vec![0u8; usize::try_from(size).unwrap()];
  let n = match snap {
    Some(id) => vol.read_in(store, id, no, 0, &mut buf).unwrap(),
    None => vol.read(store, no, 0, &mut buf).unwrap(),
  };
  buf.truncate(n);
  (buf, u64::from(nlink))
}

fn xattr_rows(vol: &Volume, store: &Store, snap: Option<SnapshotId>, no: InodeNo) -> Xattrs {
  let names = match snap {
    Some(id) => vol.xattr_names_in(store, id, no).unwrap(),
    None => vol.xattr_names(store, no).unwrap(),
  };
  names
    .into_iter()
    .map(|name| {
      let len = match snap {
        Some(id) => vol.xattr_len_in(store, id, no, &name).unwrap(),
        None => vol.xattr_len(store, no, &name).unwrap(),
      };
      let mut value = vec![0u8; usize::try_from(len).unwrap()];
      let n = match snap {
        Some(id) => vol
          .xattr_read_in(store, id, no, &name, 0, &mut value)
          .unwrap(),
        None => vol.xattr_read(store, no, &name, 0, &mut value).unwrap(),
      };
      value.truncate(n);
      (name.to_vec(), value)
    })
    .collect()
}

fn symlink_row(vol: &Volume, store: &Store, snap: Option<SnapshotId>, no: InodeNo) -> String {
  match snap {
    Some(id) => vol.readlink_in(store, id, no).unwrap().to_string(),
    None => vol.readlink(store, no).unwrap().to_string(),
  }
}
