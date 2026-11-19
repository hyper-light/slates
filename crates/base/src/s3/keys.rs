//! The keys of an S3 base mapped to a directory tree (A-464; §4.15 "An S3 base"). S3 has no directories: a key is one
//! string of up to 1,024 bytes of UTF-8, and `/` in it means nothing to S3 but what a delimited listing groups by. A
//! base presents the keys under its prefix as a POSIX tree, `/` separating its components, and a key that cannot be one
//! POSIX path is refused by its kind and named, never dropped in silence, never altered into another path:
//! - an empty component (a key with `//`, or one starting with `/` past the prefix): no POSIX name is empty;
//! - a `.` or `..` component: S3 accepts both (its object key guide) and a path would resolve them, so a key could
//!   alias another or leave the base;
//! - a NUL, which ends a POSIX name;
//! - a component longer than a name may be ([`slates_vfs::names::NAME_MAX`]), or a key longer than S3 allows
//!   ([`super::list::KEY_MAX_BYTES`], a listing no S3 sends);
//! - a key outside the base's prefix (a listing must not give one; a hostile one could);
//! - a file whose path a directory takes: the key `a` beside the key `a/b`. S3 holds both; a tree cannot, so the
//!   directory stands and the file is refused `Shadowed`, whichever the listing named first.
//!
//! A key ending in `/` is a directory of its own (the "folder" a console makes, a zero-byte object), and the key equal
//! to the prefix is the root's. The tree is ordered by path, so two hosts map one listing to one tree.

use std::collections::{BTreeMap, BTreeSet};

use slates_vfs::names::NAME_MAX;

use super::list::{KEY_MAX_BYTES, Listed};

/// Format: the separator of a key's components, as a delimited listing groups by it.
pub const SEPARATOR: char = '/';

/// Why a key names no path of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The key is not under the base's prefix.
  OutsidePrefix,
  /// The key is longer than S3 allows.
  KeyTooLong,
  /// A component is empty.
  EmptyComponent,
  /// A component is `.` or `..`.
  DotComponent,
  /// A component holds a NUL.
  Nul,
  /// A component is longer than a name may be.
  NameTooLong,
  /// A directory takes the file's path.
  Shadowed,
}

/// A key's path under the prefix, checked component by component: its components, and whether it names a directory
/// (it ends in the separator). The key equal to the prefix names the root, with no component.
fn components<'a>(prefix: &str, key: &'a str) -> Result<(Vec<&'a str>, bool), Refused> {
  if u64::try_from(key.len()).unwrap_or(u64::MAX) > KEY_MAX_BYTES {
    return Err(Refused::KeyTooLong);
  }
  let relative = key.strip_prefix(prefix).ok_or(Refused::OutsidePrefix)?;
  if relative.is_empty() {
    return Ok((Vec::new(), true));
  }
  let (relative, directory) = match relative.strip_suffix(SEPARATOR) {
    Some(inner) => (inner, true),
    None => (relative, false),
  };
  let parts: Vec<&str> = relative.split(SEPARATOR).collect();
  for part in &parts {
    check_name(part)?;
  }
  Ok((parts, directory))
}

/// Whether `name` can be one POSIX name: not empty, not `.` or `..`, no NUL, at most `NAME_MAX` bytes.
pub(crate) fn check_name(name: &str) -> Result<(), Refused> {
  if name.is_empty() {
    return Err(Refused::EmptyComponent);
  }
  if name == "." || name == ".." {
    return Err(Refused::DotComponent);
  }
  if name.contains('\0') {
    return Err(Refused::Nul);
  }
  if name.len() > NAME_MAX {
    return Err(Refused::NameTooLong);
  }
  Ok(())
}

/// The keys under a prefix as a tree: its directories and its files by path (components joined by `/`, no leading
/// one; the root is the empty path and never listed), and every key refused with its kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
  /// The directories, the root's descendants, by path.
  pub directories: BTreeSet<String>,
  /// The files by path, each with what its listing said.
  pub files: BTreeMap<String, Listed>,
  /// The keys refused, in the listing's order, each with why.
  pub refused: Vec<(String, Refused)>,
}

/// Maps `objects`, listed under `prefix` without a delimiter (every key, all pages), to a [`Tree`]: each key's
/// directories made, its file placed, a directory marker's directory made, and every key that names no path refused by
/// its kind. A file whose path a directory takes is refused `Shadowed` once every key is placed, so the listing's order
/// decides nothing.
pub fn tree_of(prefix: &str, objects: &[Listed]) -> Tree {
  let mut tree = Tree::default();
  for object in objects {
    let (parts, directory) = match components(prefix, &object.key) {
      Ok(parsed) => parsed,
      Err(refused) => {
        tree.refused.push((object.key.clone(), refused));
        continue;
      }
    };
    let mut path = String::new();
    let last = parts.len().saturating_sub(1);
    for (at, part) in parts.iter().enumerate() {
      if !path.is_empty() {
        path.push(SEPARATOR);
      }
      path.push_str(part);
      if at < last || directory {
        tree.directories.insert(path.clone());
      }
    }
    if !directory && !parts.is_empty() {
      tree.files.insert(path, object.clone());
    }
  }
  let shadowed: Vec<String> = tree
    .files
    .keys()
    .filter(|path| tree.directories.contains(*path))
    .cloned()
    .collect();
  for path in shadowed {
    if let Some(object) = tree.files.remove(&path) {
      tree.refused.push((object.key, Refused::Shadowed));
    }
  }
  tree
}

/// One directory's entries as a delimited listing (`delimiter=/`) of its prefix gives them: its files and its
/// subdirectories by name, and every key or prefix refused with its kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directory {
  /// The files by name, each with what its listing said.
  pub files: BTreeMap<String, Listed>,
  /// The subdirectories by name.
  pub directories: BTreeSet<String>,
  /// The keys and prefixes refused, each with why.
  pub refused: Vec<(String, Refused)>,
}

/// Maps one delimited listing of the directory at `prefix` (ending in `/`, or empty for the bucket's root) to its
/// entries: each object directly under it a file (the directory's own marker, the key equal to `prefix`, passed
/// over), each common prefix a subdirectory, every name checked as [`tree_of`] checks a component, and a file whose
/// name a subdirectory takes refused `Shadowed`. A key or prefix with more than one component left is refused
/// `OutsidePrefix`, since a delimited listing never gives one.
pub fn directory_of(prefix: &str, objects: &[Listed], prefixes: &[String]) -> Directory {
  let mut directory = Directory::default();
  for common in prefixes {
    match one_name(prefix, common, true) {
      Ok(Some(name)) => {
        directory.directories.insert(name.to_owned());
      }
      Ok(None) => {}
      Err(refused) => directory.refused.push((common.clone(), refused)),
    }
  }
  for object in objects {
    match one_name(prefix, &object.key, false) {
      Ok(Some(name)) if directory.directories.contains(name) => directory
        .refused
        .push((object.key.clone(), Refused::Shadowed)),
      Ok(Some(name)) => {
        directory.files.insert(name.to_owned(), object.clone());
      }
      Ok(None) => {}
      Err(refused) => directory.refused.push((object.key.clone(), refused)),
    }
  }
  directory
}

/// The one name `key` holds under `prefix`: a subdirectory's (`directory`: it ends in `/`) or a file's; `None` for the
/// directory's own marker.
fn one_name<'a>(prefix: &str, key: &'a str, directory: bool) -> Result<Option<&'a str>, Refused> {
  let (parts, marks_directory) = components(prefix, key)?;
  match parts.as_slice() {
    [] => Ok(None),
    [name] if marks_directory == directory => Ok(Some(*name)),
    // A file's key ending in `/` is a directory marker one level down, which a delimited listing rolls into a prefix;
    // and a prefix not ending in `/` names no directory. Neither comes from a listing of this directory.
    _ => Err(Refused::OutsidePrefix),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// An object listed at `key`.
  fn listed(key: &str) -> Listed {
    Listed {
      key: key.to_owned(),
      size: 1,
      etag: "\"e\"".to_owned(),
      last_modified: "2026-11-18T00:00:00.000Z".to_owned(),
    }
  }

  /// A-464: do map the keys of a base at `base/`: nested files, a directory marker, the prefix's own marker, and a key
  /// outside the prefix. Expect each file at its path with its listing, every directory on the way made, the marker's
  /// directory made though empty, the root's marker passed over, and the outsider refused `OutsidePrefix`.
  #[test]
  fn keys_under_a_prefix_map_to_their_tree() {
    let objects: Vec<Listed> = [
      "base/",
      "base/README.md",
      "base/src/lib.rs",
      "base/src/deep/er/mod.rs",
      "base/empty/",
      "other/file",
    ]
    .iter()
    .map(|key| listed(key))
    .collect();
    let tree = tree_of("base/", &objects);
    let files: Vec<&str> = tree.files.keys().map(String::as_str).collect();
    assert_eq!(files, ["README.md", "src/deep/er/mod.rs", "src/lib.rs"]);
    assert_eq!(tree.files["src/lib.rs"].key, "base/src/lib.rs");
    let directories: Vec<&str> = tree.directories.iter().map(String::as_str).collect();
    assert_eq!(directories, ["empty", "src", "src/deep", "src/deep/er"]);
    assert_eq!(
      tree.refused,
      vec![("other/file".to_owned(), Refused::OutsidePrefix)]
    );
  }

  /// A-464 (escape and alias): do map keys that name no POSIX path: an empty component, a leading separator, `.` and
  /// `..` (the escape `../etc/passwd` among them), a NUL, a component past `NAME_MAX`, and a key past S3's 1,024 bytes.
  /// Expect each refused by its kind, nothing placed for any, and the good key beside them placed.
  #[test]
  fn a_key_that_names_no_path_is_refused_by_its_kind() {
    let long_name = format!("base/{}", "n".repeat(NAME_MAX + 1));
    let long_key = format!("base/{}", "k/".repeat(600));
    let keys = [
      ("base/a//b", Refused::EmptyComponent),
      ("base//a", Refused::EmptyComponent),
      ("base/./a", Refused::DotComponent),
      ("base/../etc/passwd", Refused::DotComponent),
      ("base/a/..", Refused::DotComponent),
      ("base/a\0b", Refused::Nul),
      (long_name.as_str(), Refused::NameTooLong),
      (long_key.as_str(), Refused::KeyTooLong),
    ];
    let mut objects: Vec<Listed> = keys.iter().map(|(key, _)| listed(key)).collect();
    objects.push(listed("base/ok"));
    let tree = tree_of("base/", &objects);
    let refused: Vec<(String, Refused)> = keys
      .iter()
      .map(|(key, refused)| ((*key).to_owned(), *refused))
      .collect();
    assert_eq!(tree.refused, refused);
    assert_eq!(tree.files.keys().collect::<Vec<_>>(), ["ok"]);
    assert!(tree.directories.is_empty(), "{:?}", tree.directories);
  }

  /// A-464 (a file a directory shadows): do map the key `a` beside `a/b`, listed in both orders, and a name exactly
  /// `NAME_MAX` long. Expect the directory to stand and the file refused `Shadowed` whatever the order, and the longest
  /// name placed.
  #[test]
  fn a_file_a_directory_shadows_is_refused_whatever_the_order() {
    for keys in [["a", "a/b"], ["a/b", "a"]] {
      let objects: Vec<Listed> = keys.iter().map(|key| listed(key)).collect();
      let tree = tree_of("", &objects);
      assert_eq!(tree.files.keys().collect::<Vec<_>>(), ["a/b"]);
      assert!(tree.directories.contains("a"));
      assert_eq!(tree.refused, vec![("a".to_owned(), Refused::Shadowed)]);
    }
    let longest = "x".repeat(NAME_MAX);
    let tree = tree_of("", &[listed(&longest)]);
    assert!(tree.files.contains_key(&longest));
  }

  /// A-464 (one directory's delimited listing): do map the delimited listing of `base/src/`: files directly under it,
  /// its own marker, subdirectories as common prefixes, a file a subdirectory shadows, a `..` prefix, and a key two
  /// levels down (which no delimited listing gives). Expect the files and subdirectories by name, the marker passed
  /// over, and each other refused by its kind.
  #[test]
  fn a_delimited_listing_maps_to_one_directorys_entries() {
    let objects: Vec<Listed> = [
      "base/src/",
      "base/src/lib.rs",
      "base/src/main.rs",
      "base/src/util",
      "base/src/a/b",
    ]
    .iter()
    .map(|key| listed(key))
    .collect();
    let prefixes = vec![
      "base/src/util/".to_owned(),
      "base/src/bin/".to_owned(),
      "base/src/../".to_owned(),
    ];
    let directory = directory_of("base/src/", &objects, &prefixes);
    assert_eq!(
      directory.files.keys().collect::<Vec<_>>(),
      ["lib.rs", "main.rs"]
    );
    assert_eq!(
      directory.directories.iter().collect::<Vec<_>>(),
      ["bin", "util"]
    );
    assert_eq!(
      directory.refused,
      vec![
        ("base/src/../".to_owned(), Refused::DotComponent),
        ("base/src/util".to_owned(), Refused::Shadowed),
        ("base/src/a/b".to_owned(), Refused::OutsidePrefix),
      ]
    );
  }
}
