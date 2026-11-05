//! The restricted base's oracle (§4.16 "Composition at seal"): composing a journal over
//! `Green::base_for(version, journal)`, the base restricted to the paths the journal names, gives the
//! same result as composing it over `Green::base_at(version)`, the whole base: the same ops document,
//! or the same typed refusal. A submit builds the restricted one, so its cost follows what the work
//! changed, not what the green holds.
//!
//! The green is generated with history in every dimension (files, directories, modes, symlinks, hard
//! links, extended attributes, FIFO/socket inodes) by generated increments, some of which conflict and
//! are not committed; then generated journals, valid or not, are composed at every version from the
//! origin to the head. Paths come from a small alphabet with nesting, so journals meet the base's
//! entries, their neighbours and their children.

// Test code may panic (CLAUDE.md §2 item 6 applies to shipped code).
#![allow(clippy::indexing_slicing)]

mod common;

use common::Build;
use proptest::prelude::*;
use slates_merge::engine::Green;
use slates_merge::increment::{VolumeOp, compose_volume};
use slates_merge::origin::Origin;
use slates_merge::special::{SpecialKind, SpecialNode};

/// Shape: the path alphabet: siblings, a directory with children, and a name that prefixes another
/// (`d` and `dd`), so a range read past one path's entries is caught.
const PATHS: &[&str] = &["a", "b", "d", "d/x", "d/y", "dd", "e/f"];
/// Shape: the xattr names.
const NAMES: &[&str] = &["user.k", "user.l"];

fn path(index: u8) -> &'static str {
  PATHS[usize::from(index) % PATHS.len()]
}

fn name(index: u8) -> &'static str {
  NAMES[usize::from(index) % NAMES.len()]
}

fn node(seed: u8) -> SpecialNode {
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
type Raw = (u8, u8, u8, u8, u8);

/// An origin from generated rows, well formed as a volume is: each path is at most one kind (a file,
/// a directory, a symlink, a hard link or a FIFO/socket), the first row naming it decides which, and
/// modes and attributes sit on paths that are present.
fn origin_of(rows: &[Raw]) -> Origin {
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
fn increment_of(raw: &Raw) -> Build {
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
fn op_of(raw: &Raw) -> VolumeOp {
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

fn raw() -> impl Strategy<Value = Raw> {
  (
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
    any::<u8>(),
  )
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
  /// T-6.x (§4.16 "Composition at seal"; the submit's restricted base): for any green built by
  /// generated increments over a generated origin, and any generated journal, composing over the
  /// base restricted to the journal's paths equals composing over the whole base at every version
  /// from the origin to the head — the same document or the same refusal.
  #[test]
  fn the_restricted_base_composes_as_the_whole_base(
    origin in proptest::collection::vec(raw(), 0..24),
    history in proptest::collection::vec(raw(), 0..16),
    journals in proptest::collection::vec(proptest::collection::vec(raw(), 1..8), 1..8),
  ) {
    let mut green = Green::with_origin(&origin_of(&origin));
    for (index, step) in history.iter().enumerate() {
      let id = u8::try_from(index).unwrap_or(u8::MAX);
      // A conflicting step is not committed; the head moves only on an accept.
      let _ = green.submit(&increment_of(step).at(id, green.head()));
    }
    let mut compared = 0u32;
    for version in 0..=green.head() {
      for journal in &journals {
        let journal: Vec<VolumeOp> = journal.iter().map(op_of).collect();
        let whole = compose_volume(&green.base_at(version), &journal);
        let restricted = compose_volume(&green.base_for(version, &journal), &journal);
        prop_assert_eq!(
          restricted.as_ref().map(slates_merge::ops_doc::OpsDoc::encode),
          whole.as_ref().map(slates_merge::ops_doc::OpsDoc::encode),
          "version {} of {}, journal {:?}",
          version,
          green.head(),
          journal
        );
        compared += 1;
      }
    }
    // Non-vacuity: every version was composed at least once.
    prop_assert!(u64::from(compared) > green.head());
  }
}

/// T-6.x non-vacuity for the oracle above: the restricted base is smaller than the whole one when the
/// journal names one path of many, and still composes the same document.
#[test]
fn the_restricted_base_holds_only_what_the_journal_names() {
  let origin = Origin {
    files: (0..64).map(|n| (format!("f{n:02}"), vec![7; 8])).collect(),
    ..Origin::default()
  };
  let green = Green::with_origin(&origin);
  let journal = vec![VolumeOp::Overwrite {
    path: "f07".to_owned(),
    at: 0,
    len: 4,
  }];
  let restricted = green.base_for(0, &journal);
  assert_eq!(restricted.files, vec![("f07".to_owned(), 8)]);
  assert_eq!(green.base_at(0).files.len(), 64);
  assert_eq!(
    compose_volume(&restricted, &journal).map(|doc| doc.encode()),
    compose_volume(&green.base_at(0), &journal).map(|doc| doc.encode())
  );
}
