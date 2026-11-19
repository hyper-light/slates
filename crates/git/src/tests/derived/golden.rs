//! The golden vectors (A-664; `fixtures/made/make-made-fixtures.sh`): the trees stock git 2.50.1 wrote of the files of
//! `files.tsv`, and the commits it wrote of the cases of `commits.tsv`, in a SHA-1 and a SHA-256 repository.

use std::collections::BTreeMap;

use slates_vcs::derived::paths::Change;
use slates_vcs::{ContentId, ContentStore};

use super::super::{Commits, Fields, FileClass, Identity, Item, Trees};
use super::{FORMATS, Files, History, commits, entries, store, trees};
use crate::Admission;
use crate::object_name::{Hasher, ObjectFormat, ObjectId};
use crate::objects::Ident;
use crate::objects::commit::Commit as GitCommit;
use crate::out::Out;

/// The files the trees were written of: `formats  mode  path  bytes`, tab-separated, the bytes in hexadecimal.
const FILES: &str = include_str!("../../../fixtures/made/files.tsv");
/// The commits' cases: `case  tree  parents  seconds  name  email  message`, tab-separated, the message in hexadecimal.
const CASES: &str = include_str!("../../../fixtures/made/commits.tsv");
/// Each output's SHA-256, as `shasum -a 256` wrote it.
const SHA256SUMS: &str = include_str!("../../../fixtures/made/SHA256SUMS");
/// The git that wrote them.
const GIT_VERSION: &str = include_str!("../../../fixtures/made/git-version.txt");
/// The SHA-1 repository's trees: `id path bytes`.
const SHA1_TREES: &str = include_str!("../../../fixtures/made/sha1/trees.txt");
/// The SHA-1 repository's commits: `case id bytes`.
const SHA1_COMMITS: &str = include_str!("../../../fixtures/made/sha1/commits.txt");
/// The SHA-256 repository's trees.
const SHA256_TREES: &str = include_str!("../../../fixtures/made/sha256/trees.txt");
/// The SHA-256 repository's commits.
const SHA256_COMMITS: &str = include_str!("../../../fixtures/made/sha256/commits.txt");

/// The roots of the input's version and of the version of no files.
type Roots = (ContentId, ContentId);

/// The bytes a hexadecimal string spells.
fn unhex(hex: &str) -> Vec<u8> {
  hex
    .as_bytes()
    .as_chunks::<2>()
    .0
    .iter()
    .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
    .collect()
}

/// `bytes`' SHA-256 in lower-case hexadecimal, as `shasum -a 256` writes it.
fn sha256(bytes: &[u8]) -> String {
  let mut hasher = Hasher::raw(ObjectFormat::Sha256);
  hasher.put(bytes);
  hasher.finish().unwrap().to_string()
}

/// `path`'s SHA-256 is the one `sums` holds.
fn summed(sums: &BTreeMap<&str, &str>, (path, text): (&str, &str)) {
  assert_eq!(
    sums.get(path).copied(),
    Some(sha256(text.as_bytes()).as_str()),
    "{path}"
  );
}

/// A-664 (golden vectors). Do: hash every output of `make-made-fixtures.sh` with SHA-256. Expect: each the sum
/// `SHA256SUMS` names, every output listed there and nothing else, made by stock git 2.50.1 (Apple Git-155).
#[test]
fn t_a664_every_fixture_is_the_bytes_its_sum_names() {
  let outputs = [
    ("git-version.txt", GIT_VERSION),
    ("sha1/trees.txt", SHA1_TREES),
    ("sha1/commits.txt", SHA1_COMMITS),
    ("sha256/trees.txt", SHA256_TREES),
    ("sha256/commits.txt", SHA256_COMMITS),
  ];
  let sums: BTreeMap<&str, &str> = SHA256SUMS
    .lines()
    .map(|line| line.split_once("  ").unwrap())
    .map(|(sum, path)| (path, sum))
    .collect();
  assert_eq!(sums.len(), outputs.len());
  outputs.into_iter().for_each(|output| summed(&sums, output));
  assert_eq!(GIT_VERSION, "git version 2.50.1 (Apple Git-155)\n");
}

/// The class git's mode text `mode` names.
fn class_of(mode: &str) -> u8 {
  FileClass::ALL
    .into_iter()
    .find(|class| class.mode().text() == mode.as_bytes())
    .map(FileClass::class)
    .unwrap_or_else(|| panic!("{mode} is a mode a file has"))
}

/// The rows of a tab-separated input, its comment lines left out.
fn rows(text: &'static str) -> impl Iterator<Item = Vec<&'static str>> {
  text
    .lines()
    .filter(|line| !line.starts_with('#'))
    .map(|line| line.split('\t').collect())
}

/// The files of `files.tsv` in `format`: each path's class and bytes.
fn input_files(format: ObjectFormat) -> Files {
  rows(FILES)
    .filter(|row| row[0] == "both" || row[0] == format.name())
    .map(|row| {
      (
        row[2].as_bytes().to_vec(),
        (class_of(row[1]), unhex(row[3])),
      )
    })
    .collect()
}

/// A line of a format's `trees.txt` or `commits.txt`: its first word, its id, and its bytes.
fn golden_lines(
  text: &'static str,
  format: ObjectFormat,
) -> Vec<(&'static str, ObjectId, Vec<u8>)> {
  text
    .lines()
    .map(|line| {
      let mut words = line.splitn(3, ' ');
      let (first, second, bytes) = (
        words.next().unwrap(),
        words.next().unwrap(),
        words.next().unwrap(),
      );
      match ObjectId::from_hex(format, first.as_bytes()) {
        Ok(id) => (second, id, unhex(bytes)),
        Err(_) => (
          first,
          ObjectId::from_hex(format, second.as_bytes()).unwrap(),
          unhex(bytes),
        ),
      }
    })
    .collect()
}

/// A format's golden trees and commits.
fn golden(format: ObjectFormat) -> (&'static str, &'static str) {
  match format {
    ObjectFormat::Sha1 => (SHA1_TREES, SHA1_COMMITS),
    ObjectFormat::Sha256 => (SHA256_TREES, SHA256_COMMITS),
  }
}

/// The version of `files.tsv` in `format` built from scratch and applied as one change over no parent, which must be
/// one value; and the version of no files. The store, the trees, and the two roots.
fn input_values(format: ObjectFormat) -> (ContentStore, Trees<History>, Roots) {
  let mut store = store();
  let mut trees = trees(format);
  let files = input_files(format);
  let entries = entries(&mut store, &files);
  let built = trees.build(&mut store, &entries).unwrap();
  let change: Change = entries
    .iter()
    .map(|(path, entry)| (path.clone(), Some(*entry)))
    .collect();
  let applied = trees.apply(&mut store, None, &change).unwrap();
  assert_eq!(
    applied, built,
    "one change over nothing is the version built from scratch"
  );
  let empty = trees.build(&mut store, &BTreeMap::new()).unwrap();
  (store, trees, (built, empty))
}

/// The directory node a golden path names in the input's version (`.` its root, `-` the version of no files).
fn node_at(
  (store, trees): (&ContentStore, &Trees<History>),
  (root, empty): Roots,
  path: &str,
) -> ContentId {
  match path {
    "." => root,
    "-" => empty,
    path => match trees.get(store, root, path.as_bytes()).unwrap() {
      Some(Item::Tree { node, .. }) => node,
      other => panic!("{path} is a directory, not {other:?}"),
    },
  }
}

/// The tree at `path` is the one stock git wrote: its id and its bytes.
fn tree_as_git_wrote(
  (store, trees): (&ContentStore, &Trees<History>),
  roots: Roots,
  (path, id, bytes): (&str, ObjectId, Vec<u8>),
) {
  let node = node_at((store, trees), roots, path);
  let directory = trees.directory(store, node).unwrap();
  assert_eq!(directory.id, id, "{path}");
  assert_eq!(
    store.object_bytes(&directory.object, directory.len),
    Some(bytes),
    "{path}"
  );
}

/// A-664 (golden vectors; §4.17.2 item 5: modes from the files' kinds, git's entry order). Do: make each format's
/// version of `files.tsv` (a file, an executable, a symbolic link, a submodule's commit, an empty file and a binary
/// one, names that bytes and git order differently, `foo-bar`, `foo.c`, `foo/` and `foo0`, and `sub.txt` before
/// `sub/`), from scratch and as one change over no parent; and the version of no files. Expect: the two one value, and
/// every tree in it, the root and each of its four directories, the id and the bytes stock git 2.50.1 wrote through
/// `git update-index --cacheinfo` and `git write-tree`; the empty version's the tree `git mktree` wrote of nothing.
#[test]
fn t_a664_the_trees_are_the_trees_stock_git_wrote() {
  for format in FORMATS {
    let (store, trees, roots) = input_values(format);
    let goldens = golden_lines(golden(format).0, format);
    assert_eq!(
      goldens.len(),
      1 + 4 + 1,
      "the root, its four directories and the empty tree"
    );
    for line in goldens {
      tree_as_git_wrote((&store, &trees), roots, line);
    }
  }
}

/// The name and email git wrote as a commit's author.
fn written_identity(bytes: &[u8]) -> (String, String) {
  let commit = GitCommit::parse(bytes, &mut Admission::new(u64::MAX)).unwrap();
  let ident = Ident::parse(commit.author().unwrap()).unwrap();
  let text = |bytes: &[u8]| String::from_utf8(bytes.to_vec()).unwrap();
  (text(ident.name), text(ident.email))
}

/// One case of `commits.tsv`, and what stock git wrote of it.
struct Case {
  name: &'static str,
  tree: &'static str,
  parents: Vec<&'static str>,
  seconds: u64,
  identity: (&'static str, &'static str),
  message: String,
  written: (ObjectId, Vec<u8>),
}

/// The cases of `commits.tsv` in `format`, in order, each with the commit stock git wrote of it.
fn cases(format: ObjectFormat) -> Vec<Case> {
  let written: BTreeMap<&str, (ObjectId, Vec<u8>)> = golden_lines(golden(format).1, format)
    .into_iter()
    .map(|(case, id, bytes)| (case, (id, bytes)))
    .collect();
  rows(CASES)
    .map(|row| Case {
      name: row[0],
      tree: row[1],
      parents: row[2].split(',').filter(|parent| *parent != "-").collect(),
      seconds: row[3].parse().unwrap(),
      identity: (row[4], if row[5] == "-" { "" } else { row[5] }),
      message: String::from_utf8(unhex(row[6])).unwrap(),
      written: written[row[0]].clone(),
    })
    .collect()
}

/// The commit of `case` is the one stock git wrote: made of the identity git wrote, which `Identity` takes as it is,
/// while the case's own identity is taken only where git wrote it unchanged. Its id.
fn commit_as_git_wrote(
  (store, trees, commits): (&mut ContentStore, &Trees<History>, &mut Commits<History>),
  (roots, made): (Roots, &BTreeMap<&str, ObjectId>),
  case: &Case,
) -> ObjectId {
  let (name, email) = written_identity(&case.written.1);
  let unchanged = (name.as_str(), email.as_str()) == case.identity;
  assert_eq!(
    Identity::new(case.identity.0, case.identity.1).is_ok(),
    unchanged,
    "{}",
    case.name
  );
  let fields = Fields {
    tree: node_at((&*store, trees), roots, case.tree),
    identity: Identity::new(&name, &email).unwrap(),
    seconds: case.seconds,
    message: &case.message,
  };
  let parents: Vec<ObjectId> = case.parents.iter().map(|parent| made[parent]).collect();
  let node = commits.make(store, &fields, &parents).unwrap();
  assert_eq!(
    commits.id(store, node).unwrap(),
    case.written.0,
    "{}",
    case.name
  );
  assert_eq!(
    commits.object(store, node).unwrap(),
    Some(case.written.1.clone()),
    "{}",
    case.name
  );
  case.written.0
}

/// A-664 (golden vectors; §4.17.1 item 4's commit: the tree, the parents, author and committer of the principal's
/// identity at the merge record's time, the submit's message). Do: make each case of `commits.tsv` in each format: a
/// root commit, one over it, a merge of both, one of the empty tree with an empty message, one of UTF-8 identity and
/// message, one whose message's last line ends and one whose ends in blank lines, and the cases where git changed the
/// identity it was given (crud at both ends, `<` and `>` inside) or wrote an empty email. Expect: each commit's id and
/// bytes the ones stock git 2.50.1's `git commit-tree -m` wrote, made of the identity git wrote; and the identity a
/// case gave refused exactly where git did not write it as given.
#[test]
fn t_a664_the_commits_are_the_commits_stock_git_wrote() {
  for format in FORMATS {
    let (mut store, trees, roots) = input_values(format);
    let mut commits = commits(format);
    let mut made: BTreeMap<&str, ObjectId> = BTreeMap::new();
    let cases = cases(format);
    assert_eq!(cases.len(), 8);
    for case in &cases {
      let id = commit_as_git_wrote((&mut store, &trees, &mut commits), (roots, &made), case);
      made.insert(case.name, id);
    }
  }
}
