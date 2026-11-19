//! The report fixtures' repository, built for the drive's tests (A-616, part 2): `history.txt`
//! read as `make-fixtures.py` reads it, each revision's `svnmucc` actions made the changed-path
//! rows FSFS records (one a path: an addition, a deletion, a replacement or a modification, with
//! the text and the property list it leaves), appended to an index (A-553), and the same history
//! kept as trees for a green. So the index, the green and the `svnserve` the sessions were recorded
//! against hold one history, which the tests check against `changed.txt` (`svnlook changed`).

use super::Guards;
use crate::report::Report;
use crate::report::recorded::Written;
use crate::svndiff::{Window, apply_all};
use crate::index::{
  Action, ChangedPath, ContentIdentity, CopySource, Index, NodeKind, PropertyList, Revision,
  RevisionRecord, Text,
};
use crate::mergeinfo::Contents;
use crate::reads::Repository;
use aws_lc_rs::digest::{MD5_FOR_LEGACY_USE_ONLY, SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use std::collections::BTreeMap;

/// The history every report fixture was recorded against.
pub(crate) const HISTORY: &str = include_str!("../../../fixtures/report/history.txt");

/// `svnlook changed` of every revision, as the recorder kept it.
pub(crate) const CHANGED: &str = include_str!("../../../fixtures/report/changed.txt");

/// The text of a `lines:` file's line (`make-fixtures.py` `LINE`).
const LINE: &str = " of a text long enough to need several windows\n";

/// The text of an edited line (`make-fixtures.py` `EDITED_LINE`).
const EDITED_LINE: &str = ", edited on the branch of history\n";

/// The transaction name every revision is recorded with.
const TRANSACTION: &str = "0-1";

/// The most bytes of a text the green hands over at once: less than a window, so a window is
/// gathered from several runs, as from a chunked text.
const PIECE: usize = 1_000;

/// The property holding mergeinfo.
const MERGEINFO: &str = "svn:mergeinfo";

/// One node of the green.
#[derive(Clone, Debug, Default)]
struct GreenNode {
  /// Whether it is a directory.
  directory: bool,
  /// Its properties.
  properties: BTreeMap<String, Vec<u8>>,
  /// Its text.
  text: Vec<u8>,
}

/// What one revision did to one path so far.
#[derive(Clone, Debug)]
struct Row {
  /// The row's action.
  action: Action,
  /// The copy source.
  copy_from: Option<CopySource>,
  /// Whether the text was set.
  text: bool,
  /// Whether the properties were set.
  properties: bool,
  /// Whether `svn:mergeinfo` was set or removed.
  mergeinfo: bool,
}

/// The green of the fixtures' repository: every revision's tree, the property lists and texts by
/// identity, and every revision's properties.
#[derive(Debug, Default)]
pub(crate) struct Green {
  /// Every revision's whole tree, by repository path.
  trees: Vec<BTreeMap<String, GreenNode>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// The texts, by identity.
  pub(crate) texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// Every revision's properties.
  revisions: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
}

impl Contents for Green {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    let tree = self.trees.get(usize::try_from(revision).ok()?)?;
    if !tree.get(directory)?.directory {
      return None;
    }
    let prefix = if directory == "/" {
      String::from("/")
    } else {
      format!("{directory}/")
    };
    Some(
      tree
        .keys()
        .filter_map(|place| place.strip_prefix(&prefix))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .map(str::to_owned)
        .collect(),
    )
  }

  fn mergeinfo_value(&self, properties: &ContentIdentity) -> Option<&[u8]> {
    self
      .lists
      .get(properties)?
      .iter()
      .find(|(name, _)| name == MERGEINFO.as_bytes())
      .map(|(_, value)| value.as_slice())
  }
}

impl Repository for Green {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    let properties = self.revisions.get(usize::try_from(revision).ok()?)?;
    Some(
      properties
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    )
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    let list = self.lists.get(identity)?;
    Some(
      list
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    )
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    let text = self.texts.get(identity)?;
    let start = usize::try_from(offset).ok()?;
    text.get(start..start.saturating_add(PIECE).min(text.len()))
  }
}

/// The session's guards in the fixtures: what the anonymous principal may not read
/// (`make-fixtures.py` `AUTHZ`: `/trunk/secret` and beneath), and the locks held now.
#[derive(Clone, Debug, Default)]
pub(crate) struct FixtureGuards {
  /// The repository paths hidden, each with everything beneath it.
  pub(crate) hidden: Vec<String>,
  /// The locks held, by path: their tokens.
  pub(crate) locks: BTreeMap<String, Vec<u8>>,
}

impl FixtureGuards {
  /// The anonymous principal's guards with no lock held.
  pub(crate) fn anonymous() -> Self {
    Self {
      hidden: vec![String::from("/trunk/secret")],
      locks: BTreeMap::new(),
    }
  }
}

impl Guards for FixtureGuards {
  fn lock_token(&self, path: &str) -> Option<&[u8]> {
    self.locks.get(path).map(Vec::as_slice)
  }

  fn may_read(&self, path: &str) -> bool {
    !self.hidden.iter().any(|hidden| at_or_beneath(path, hidden))
  }
}

/// Whether `place` is `path` or beneath it.
fn at_or_beneath(place: &str, path: &str) -> bool {
  place == path
    || path == "/"
    || place
      .strip_prefix(path)
      .is_some_and(|rest| rest.starts_with('/'))
}

/// A digest of `bytes` as a fixed array.
fn digest_of<const N: usize>(
  algorithm: &'static aws_lc_rs::digest::Algorithm,
  bytes: &[u8],
) -> [u8; N] {
  digest(algorithm, bytes).as_ref().try_into().unwrap()
}

/// The MD5 of `bytes`.
pub(crate) fn md5(bytes: &[u8]) -> [u8; 16] {
  digest_of(&MD5_FOR_LEGACY_USE_ONLY, bytes)
}

/// `history.txt`'s escapes: `\n`, `\t`, `\\` and `\xHH`.
fn unescape(text: &str) -> Vec<u8> {
  let mut out = Vec::new();
  let mut chars = text.chars();
  while let Some(next) = chars.next() {
    if next != '\\' {
      out.extend_from_slice(next.encode_utf8(&mut [0; 4]).as_bytes());
      continue;
    }
    match chars.next() {
      Some('n') => out.push(b'\n'),
      Some('t') => out.push(b'\t'),
      Some('\\') => out.push(b'\\'),
      Some('x') => {
        let hex: String = chars.by_ref().take(2).collect();
        out.push(u8::from_str_radix(&hex, 16).unwrap());
      }
      other => panic!("an unknown escape {other:?}"),
    }
  }
  out
}

/// The bytes a `put` names (`make-fixtures.py` `content`).
fn content(spec: &str) -> Vec<u8> {
  if let Some(count) = spec.strip_prefix("bytes:") {
    return (0..count.parse::<u16>().unwrap())
      .map(|byte| u8::try_from(byte).unwrap())
      .collect();
  }
  if let Some(lines) = spec.strip_prefix("lines:") {
    let parts: Vec<&str> = lines.split(':').collect();
    let count: usize = parts[0].parse().unwrap();
    let edited: Option<usize> =
      (parts.len() == 3 && parts[1] == "edit").then(|| parts[2].parse().unwrap());
    return (1..=count)
      .map(|line| {
        let rest = if Some(line) == edited {
          EDITED_LINE
        } else {
          LINE
        };
        format!("line {line}{rest}")
      })
      .collect::<String>()
      .into_bytes();
  }
  unescape(spec)
}

/// The fixtures' repository: its index and its green.
pub(crate) fn repository() -> (Index, Green) {
  let mut index = Index::new();
  let mut green = Green::default();
  let root = GreenNode {
    directory: true,
    ..GreenNode::default()
  };
  green
    .trees
    .push(BTreeMap::from([(String::from("/"), root)]));
  green.revisions.push(Vec::new());
  for block in HISTORY.split("\nrevision ").skip(1).map(|block| {
    block
      .lines()
      .filter(|line| !line.is_empty() && !line.starts_with('#'))
  }) {
    green.commit(block.collect(), &mut index);
  }
  (index, green)
}

impl Green {
  /// One revision: its header line, then its actions.
  fn commit(&mut self, lines: Vec<&str>, index: &mut Index) {
    let revision = Revision::try_from(self.trees.len()).unwrap();
    let header: Vec<&str> = lines[0].split(' ').collect();
    self.revisions.push(vec![
      (b"svn:author".to_vec(), header[0].as_bytes().to_vec()),
      (b"svn:date".to_vec(), header[1].as_bytes().to_vec()),
      (b"svn:log".to_vec(), format!("r{revision}").into_bytes()),
    ]);
    let mut tree = self.trees.last().unwrap().clone();
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for line in &lines[1..] {
      self.act(line, revision, &mut tree, &mut rows);
    }
    let changes = rows
      .into_iter()
      .map(|(path, row)| self.changed_path(&tree, path, row))
      .collect();
    let record = RevisionRecord {
      transaction: String::from(TRANSACTION),
      changes,
    };
    index.append(record, usize::MAX).unwrap();
    self.trees.push(tree);
  }

  /// One `svnmucc` action, applied to the revision's tree and noted in its rows.
  fn act(
    &mut self,
    line: &str,
    revision: Revision,
    tree: &mut BTreeMap<String, GreenNode>,
    rows: &mut BTreeMap<String, Row>,
  ) {
    let words: Vec<&str> = line.split(' ').collect();
    let path = |word: &str| format!("/{word}");
    match words[0] {
      "mkdir" => {
        let node = GreenNode {
          directory: true,
          ..GreenNode::default()
        };
        tree.insert(path(words[1]), node);
        added(rows, path(words[1]), None);
      }
      "put" => self.put(tree, rows, &path(words[1]), content(&words[2..].join(" "))),
      "rm" => {
        remove(tree, &path(words[1]));
        rows.insert(path(words[1]), row(Action::Delete, None));
      }
      "cp" => {
        let from = revision_of(words[1]);
        self.copy(tree, rows, &path(words[2]), from, &path(words[3]));
      }
      "mv" => {
        self.copy(tree, rows, &path(words[1]), revision - 1, &path(words[2]));
        remove(tree, &path(words[1]));
        rows.insert(path(words[1]), row(Action::Delete, None));
      }
      "propset" => set_property(tree, rows, &path(words[3]), words[1], Some(words[2])),
      "propdel" => set_property(tree, rows, &path(words[2]), words[1], None),
      other => panic!("an unknown action {other}"),
    }
  }

  /// `put`: a new file's text, or an existing one's changed.
  fn put(
    &mut self,
    tree: &mut BTreeMap<String, GreenNode>,
    rows: &mut BTreeMap<String, Row>,
    path: &str,
    text: Vec<u8>,
  ) {
    match tree.get_mut(path) {
      Some(node) => node.text = text,
      None => {
        tree.insert(
          path.to_owned(),
          GreenNode {
            text,
            ..GreenNode::default()
          },
        );
        added(rows, path.to_owned(), None);
      }
    }
    rows
      .entry(path.to_owned())
      .or_insert_with(|| row(Action::Modify, None))
      .text = true;
  }

  /// `cp` and `mv`'s copy: the source's tree as it stood in `from`, at `to`.
  fn copy(
    &self,
    tree: &mut BTreeMap<String, GreenNode>,
    rows: &mut BTreeMap<String, Row>,
    source: &str,
    from: Revision,
    to: &str,
  ) {
    let old = &self.trees[usize::try_from(from).unwrap()];
    for (place, node) in old.iter().filter(|(place, _)| at_or_beneath(place, source)) {
      let moved = format!("{to}{}", place.get(source.len()..).unwrap());
      tree.insert(moved, node.clone());
    }
    let copy = CopySource {
      path: source.to_owned(),
      revision: from,
    };
    added(rows, to.to_owned(), Some(copy));
  }

  /// A path's row, with the text and the property list it leaves.
  fn changed_path(
    &mut self,
    tree: &BTreeMap<String, GreenNode>,
    path: String,
    row: Row,
  ) -> ChangedPath {
    let node = tree.get(&path);
    let kind = node.map_or(NodeKind::File, |node| {
      if node.directory {
        NodeKind::Directory
      } else {
        NodeKind::File
      }
    });
    let text =
      (row.text && row.action != Action::Delete).then(|| self.text_of(&node.unwrap().text));
    let properties = (row.properties && row.action != Action::Delete)
      .then(|| self.list_of(&node.unwrap().properties));
    ChangedPath {
      kind: if row.action == Action::Delete {
        self.deleted_kind(&path)
      } else {
        kind
      },
      path,
      action: row.action,
      copy_from: row.copy_from,
      text,
      properties,
      mergeinfo_modified: row.mergeinfo,
    }
  }

  /// The kind of the node a deletion removes, as the previous revision held it.
  fn deleted_kind(&self, path: &str) -> NodeKind {
    let node = &self.trees.last().unwrap()[path];
    if node.directory {
      NodeKind::Directory
    } else {
      NodeKind::File
    }
  }

  /// A text held by its identity, with its digests.
  fn text_of(&mut self, bytes: &[u8]) -> Text {
    let identity: ContentIdentity = digest_of(&SHA256, bytes);
    self.texts.insert(identity, bytes.to_vec());
    Text {
      identity,
      md5: md5(bytes),
      sha1: digest_of(&SHA1_FOR_LEGACY_USE_ONLY, bytes),
      size: u64::try_from(bytes.len()).unwrap(),
    }
  }

  /// A property list held by its identity.
  fn list_of(&mut self, properties: &BTreeMap<String, Vec<u8>>) -> PropertyList {
    if properties.is_empty() {
      return PropertyList::Empty;
    }
    let mut encoded = Vec::new();
    for (name, value) in properties {
      encoded.extend_from_slice(&u64::try_from(name.len()).unwrap().to_le_bytes());
      encoded.extend_from_slice(name.as_bytes());
      encoded.extend_from_slice(&u64::try_from(value.len()).unwrap().to_le_bytes());
      encoded.extend_from_slice(value);
    }
    let identity: ContentIdentity = digest_of(&SHA256, &encoded);
    let list = properties
      .iter()
      .map(|(name, value)| (name.as_bytes().to_vec(), value.clone()))
      .collect();
    self.lists.insert(identity, list);
    PropertyList::Held {
      identity,
      has_mergeinfo: properties.contains_key(MERGEINFO),
    }
  }
}

/// A row of `action` with nothing set.
fn row(action: Action, copy_from: Option<CopySource>) -> Row {
  Row {
    action,
    copy_from,
    text: false,
    properties: false,
    mergeinfo: false,
  }
}

/// A path added: an addition, or a replacement of what the revision deleted there.
fn added(rows: &mut BTreeMap<String, Row>, path: String, copy_from: Option<CopySource>) {
  let action = match rows.get(&path) {
    Some(Row {
      action: Action::Delete,
      ..
    }) => Action::Replace,
    _ => Action::Add,
  };
  rows.insert(path, row(action, copy_from));
}

/// A path and everything beneath it removed from the tree.
fn remove(tree: &mut BTreeMap<String, GreenNode>, path: &str) {
  tree.retain(|place, _| !at_or_beneath(place, path));
}

/// `propset` or `propdel` on a path.
fn set_property(
  tree: &mut BTreeMap<String, GreenNode>,
  rows: &mut BTreeMap<String, Row>,
  path: &str,
  name: &str,
  value: Option<&str>,
) {
  let node = tree.get_mut(path).unwrap();
  match value {
    Some(value) => node
      .properties
      .insert(name.to_owned(), value.as_bytes().to_vec()),
    None => node.properties.remove(name),
  };
  let row = rows
    .entry(path.to_owned())
    .or_insert_with(|| row(Action::Modify, None));
  row.properties = true;
  row.mergeinfo |= name == MERGEINFO;
}

/// A revision named in `history.txt`.
fn revision_of(word: &str) -> Revision {
  word.parse().unwrap()
}

/// The prefix that begins every entry property's name.
pub(crate) const ENTRY_PREFIX: &str = "svn:entry:";

/// A drive's calls in a form two drives of one edit share.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Normal {
  /// A call as it was.
  Call(Written),
  /// A run of property changes on one node: the entry properties in their order, then the rest
  /// by name.
  Properties(Vec<(String, Option<Vec<u8>>)>),
  /// A run of deletions, by path.
  Deletions(Vec<(String, Option<u64>)>),
  /// A delta: the base's MD5, and the MD5 and length of what it makes of the base.
  Delta(Option<[u8; 16]>, [u8; 16], usize),
}

/// The calls in their shared form, every delta applied to the text whose MD5 it names.
pub(crate) fn normalize(calls: Vec<Written>, texts: &BTreeMap<[u8; 16], Vec<u8>>) -> Vec<Normal> {
  let mut normal = Vec::new();
  for call in calls {
    match call {
      Written::ChangeProperty(name, value) => match normal.last_mut() {
        Some(Normal::Properties(run)) => run.push((name, value)),
        _ => normal.push(Normal::Properties(vec![(name, value)])),
      },
      Written::Delete(path, revision) => match normal.last_mut() {
        Some(Normal::Deletions(run)) => run.push((path, revision)),
        _ => normal.push(Normal::Deletions(vec![(path, revision)])),
      },
      Written::Delta(base, _, windows) => normal.push(applied(base, &windows, texts)),
      call => normal.push(Normal::Call(call)),
    }
  }
  for run in &mut normal {
    match run {
      Normal::Properties(properties) => sort_own_properties(properties),
      Normal::Deletions(deletions) => deletions.sort(),
      _ => {}
    }
  }
  normal
}

/// A run of property changes with its entry properties kept in order and the rest sorted by name.
pub(crate) fn sort_own_properties(properties: &mut [(String, Option<Vec<u8>>)]) {
  let entry = properties
    .iter()
    .take_while(|(name, _)| name.starts_with(ENTRY_PREFIX))
    .count();
  properties[entry..].sort();
}

/// A delta applied to the text whose MD5 it names, none naming no bytes.
pub(crate) fn applied(
  base: Option<[u8; 16]>,
  windows: &[Window],
  texts: &BTreeMap<[u8; 16], Vec<u8>>,
) -> Normal {
  let source = base.map_or_else(Vec::new, |md5| texts[&md5].clone());
  let made = apply_all(&source, windows).unwrap();
  Normal::Delta(base, md5(&made), made.len())
}

/// The fixtures' texts by MD5, the empty one among them.
pub(crate) fn texts_by_md5(green: &Green) -> BTreeMap<[u8; 16], Vec<u8>> {
  let mut texts: BTreeMap<[u8; 16], Vec<u8>> = green
    .texts
    .values()
    .map(|text| (md5(text), text.clone()))
    .collect();
  texts.insert(md5(b""), Vec::new());
  texts
}

/// The guards a recorded session's drive ran under: the anonymous principal's, and in the session
/// that holds `a.txt`'s lock, the lock its report names.
pub(crate) fn guards_of(name: &str, report: &Report) -> FixtureGuards {
  let mut guards = FixtureGuards::anonymous();
  if name == "update-with-lock.1" {
    let held = report
      .paths
      .iter()
      .find_map(|info| info.lock_token.clone())
      .unwrap();
    guards.locks.insert(String::from("/trunk/a.txt"), held);
  }
  guards
}
