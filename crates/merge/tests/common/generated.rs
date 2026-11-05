//! Generated greens and journals shared by the oracles over them (`tests/base_for.rs`, `tests/identity.rs`): a
//! well-formed origin, single-operation increments over every dimension, and journal operations valid or not, over a
//! small path alphabet with nesting.

use proptest::prelude::*;
use slates_merge::increment::VolumeOp;
use slates_merge::origin::Origin;
use slates_merge::special::{SpecialKind, SpecialNode};

use super::Build;

/// Shape: the path alphabet: siblings, a directory with children, and a name that prefixes another
/// (`d` and `dd`), so a range read past one path's entries is caught.
const PATHS: &[&str] = &["a", "b", "d", "d/x", "d/y", "dd", "e/f"];
/// Shape: the xattr names.
const NAMES: &[&str] = &["user.k", "user.l"];

pub(crate) fn path(index: u8) -> &'static str {
  PATHS[usize::from(index) % PATHS.len()]
}

pub(crate) fn name(index: u8) -> &'static str {
  NAMES[usize::from(index) % NAMES.len()]
}

pub(crate) fn node(seed: u8) -> SpecialNode {
  SpecialNode {
    kind: if seed.is_multiple_of(2) {
      SpecialKind::Fifo
    } else {
      SpecialKind::Socket
    },
    uid: u32::from(seed),
    gid: u32::from(seed),
    atime: 1,
    mtime: 2,
    ctime: 3,
    btime: 4,
  }
}

/// One generated step: a kind, two path indices, a small number and a byte.
pub(crate) type Raw = (u8, u8, u8, u8, u8);

/// An origin from generated rows, well formed as a volume is: each path is at most one kind (a file,
/// a directory, a symlink, a hard link or a FIFO/socket), the first row naming it decides which, and
/// modes and attributes sit on paths that are present.
pub(crate) fn origin_of(rows: &[Raw]) -> Origin {
  let mut origin = Origin::default();
  let mut kinds: std::collections::BTreeMap<&str, u8> = std::collections::BTreeMap::new();
  for (kind, first, second, number, byte) in rows {
    let (here, there) = (path(*first), path(*second));
    let kind = kind % 7;
    if kind == 2 || kind == 5 {
      continue;
    }
    if kinds.contains_key(here) {
      continue;
    }
    kinds.insert(here, kind);
    match kind {
      0 => origin
        .files
        .push((here.to_owned(), vec![*byte; usize::from(*number % 16)])),
      1 => origin.dirs.push(here.to_owned()),
      3 => origin.symlinks.push((here.to_owned(), there.to_owned())),
      4 => origin.hardlinks.push((here.to_owned(), there.to_owned())),
      _ => origin.specials.push((here.to_owned(), node(*byte))),
    }
  }
  for (kind, first, _, number, byte) in rows {
    let here = path(*first);
    if !kinds.contains_key(here) {
      continue;
    }
    match kind % 7 {
      2 if !origin.modes.iter().any(|(p, _)| p == here) => {
        origin
          .modes
          .push((here.to_owned(), 0o600 | u32::from(*byte)));
      }
      5 if !origin
        .xattrs
        .iter()
        .any(|(p, n, _)| p == here && n == name(*number)) =>
      {
        origin
          .xattrs
          .push((here.to_owned(), name(*number).to_owned(), vec![*byte]));
      }
      _ => {}
    }
  }
  origin.files.sort_by(|a, b| a.0.cmp(&b.0));
  origin.dirs.sort();
  origin.modes.sort_by(|a, b| a.0.cmp(&b.0));
  origin.symlinks.sort_by(|a, b| a.0.cmp(&b.0));
  origin.hardlinks.sort_by(|a, b| a.0.cmp(&b.0));
  origin
    .xattrs
    .sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
  origin.specials.sort_by(|a, b| a.0.cmp(&b.0));
  origin
}

/// A generated increment over the head: one operation of one dimension.
pub(crate) fn increment_of(raw: &Raw) -> Build {
  let (kind, first, second, number, byte) = *raw;
  let (here, there) = (path(first), path(second));
  let mut build = Build::new();
  match kind % 10 {
    0 => build.create(here, &[byte; 3]),
    1 => build.overwrite(here, u64::from(number % 8), &[byte; 2]),
    2 => build.rename(here, there),
    3 => build.symlink(here, there),
    4 => build.link(here, there),
    5 => build.remove(here),
    6 => build.mkdir(here),
    7 => build.rmdir(here),
    8 => build.setmode(here, 0o640 | u32::from(byte)),
    _ => build.setxattr(here, name(number), &[byte]),
  };
  build
}

/// A generated journal operation, valid or not: the deriver's refusals are compared too.
pub(crate) fn op_of(raw: &Raw) -> VolumeOp {
  let (kind, first, second, number, byte) = *raw;
  let (here, there) = (path(first).to_owned(), path(second).to_owned());
  let at = u64::from(number % 12);
  let len = u64::from(byte % 6);
  match kind % 18 {
    0 => VolumeOp::Overwrite {
      path: here,
      at,
      len,
    },
    1 => VolumeOp::Extend {
      path: here,
      at,
      len,
    },
    2 => VolumeOp::Truncate {
      path: here,
      len: at,
    },
    3 => VolumeOp::Insert {
      path: here,
      at,
      len,
    },
    4 => VolumeOp::Delete {
      path: here,
      at,
      len,
    },
    5 => VolumeOp::Create { path: here },
    6 => VolumeOp::Unlink { path: here },
    7 => VolumeOp::Rename {
      from: here,
      to: there,
    },
    8 => VolumeOp::Mkdir { path: here },
    9 => VolumeOp::Rmdir { path: here },
    10 => VolumeOp::SetMode {
      path: here,
      mode: 0o600 | u32::from(byte),
    },
    11 => VolumeOp::Symlink {
      path: here,
      target: there,
    },
    12 => VolumeOp::SetXattr {
      path: here,
      name: name(number).to_owned(),
      value: vec![byte],
    },
    13 => VolumeOp::RemoveXattr {
      path: here,
      name: name(number).to_owned(),
    },
    14 => VolumeOp::Link {
      path: here,
      target: there,
    },
    15 => VolumeOp::Mknod {
      path: here,
      node: node(byte),
      mode: 0o600,
    },
    // Content on a fresh file is the common journal: weight it.
    _ => VolumeOp::Insert {
      path: here,
      at: 0,
      len,
    },
  }
}

pub(crate) fn raw() -> impl Strategy<Value = Raw> {
  (
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
  )
}
