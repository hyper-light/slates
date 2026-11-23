//! `lookup` (7.2's `wireprotov1server.lookup` over `scmutil.revsymbol`): a name resolved to the node it names in the
//! served view, answered `1 <node>\n`, or `0 <why>\n` in 7.2's words.
//!
//! **The order 7.2 tries** (`revsymbol`): `.`, `tip` and `null`; a revision number written as `%d` writes it, a
//! negative one counted from the end; 40 hexadecimal digits naming a held node; then the names' namespaces in 7.2's
//! order (`namespaces.py`): a bookmark, a global tag (`crate::tags`, the `.hgtags` of the view's heads, oldest first),
//! and a branch's tip (its last open head, else its last head); a node's hexadecimal prefix, a leading `x` dropped,
//! among the held nodes and the null node (7.2's radix tree holds it); else `unknown revision '<name>'`. A prefix more
//! than one node starts with is `00changelog@<prefix>: ambiguous identifier`. A tag naming a node the view does not
//! serve stops the walk as unknown, as 7.2's `revsymbol` refuses it rather than trying a branch. Recorded from stock
//! 7.2 (`hg serve --stdio`, `hg log -r`). An owner's `--from` asks the same order (A-575, [`resolve_name`]).
//!
//! **The tags are read from the node's changeset store** (A-767 C1, main's ruling (A) of 2026-11-21): a name that
//! reaches the tag namespace ([`needs_tags`]) has the `.hgtags` of the view's heads read first, once for a lookup or a
//! whole `batch`, by [`TagsRead`]: the rows of every head's `.hgtags` revision in one want, then their texts in one,
//! bounded by the asking step's room. A caller answering a step at a time asks the store between steps (the edges'
//! `StoreWants`); a whole answer reads them at once where its store can ([`TagsRead::now`], the tests' stand-ins), and
//! is refused typed where it cannot. A `.hgtags` past the room is never read whole: the store stops at the room and the
//! lookup is refused past its admission.
//!
//! **Apart from 7.2.** `.` is the null node: a served repository has no working directory, so no parent; and it has no
//! local tags (`.hg/localtags`), so the global ones are all it holds.

use std::collections::HashSet;

use super::discovery::{branch_head_revs, head_revs};
use super::getbundle::{Given, Store};
use super::{Refusal, Repository, Rev, node_of};
use crate::pushkey::{encode_hex, hex_node};
use crate::render::{StoredRevision, Want};
use crate::tags::Tag;
use crate::{Admission, NULL_NODE, Node};

/// Format: the answer's flag for a name resolved.
const FOUND: &[u8] = b"1 ";
/// Format: the answer's flag for a name not resolved.
const NOT_FOUND: &[u8] = b"0 ";
/// Format: the changelog's name in 7.2's ambiguous-prefix words (`revlog.display_id`).
const CHANGELOG: &[u8] = b"00changelog@";
/// Format: 7.2's words after an ambiguous prefix.
const AMBIGUOUS: &[u8] = b": ambiguous identifier";
/// Format: 7.2's words for a name it does not know, before and after the name.
const UNKNOWN: (&[u8], &[u8]) = (b"unknown revision '", b"'");

/// What a name resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolved {
  /// The node it names.
  Node(Node),
  /// A prefix more than one node starts with: the prefix.
  Ambiguous(Vec<u8>),
  /// Nothing.
  Unknown,
}

/// Format: the path of the file Mercurial keeps its global tags in.
pub(crate) const TAGS_PATH: &[u8] = b".hgtags";

/// The `.hgtags` of the served view's heads, read from the node's changeset store (module docs): the distinct file
/// revisions the heads' manifests name, oldest head first (7.2's `_filterfnodes`); then their rows, asked in one want;
/// then their texts, asked in one, within the room the reading began in; each text's metadata block left out
/// (7.2's `filectx.data()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagsRead {
  /// The `.hgtags` file revisions, in the order read.
  files: Vec<Node>,
  /// What the next read asks of the store.
  want: Option<Want>,
  /// Each text, in `files`' order, once given.
  texts: Option<Vec<Vec<u8>>>,
  /// The most bytes the texts may hold together: the room the reading began in.
  most: u64,
  /// Why the read failed, if it did: refused where the tags are read.
  refused: Option<Refusal>,
}

impl TagsRead {
  /// Begins reading the `.hgtags` of `repo`'s heads, within `admission`'s room: what [`TagsRead::wants`] asks first.
  pub fn begin(repo: &impl Repository, admission: &mut Admission) -> Result<TagsRead, Refusal> {
    let heads = head_revs(repo, admission)?;
    admission.charge_items::<Node>(heads.len().saturating_mul(2))?;
    let mut seen: HashSet<Node> = HashSet::with_capacity(heads.len());
    let mut files = Vec::with_capacity(heads.len());
    for head in heads {
      if let Some(file) = repo.tags_file(head, admission)?
        && seen.insert(file)
      {
        files.push(file);
      }
    }
    let (want, texts) = if files.is_empty() {
      (None, Some(Vec::new()))
    } else {
      let logs = vec![(TAGS_PATH.to_vec(), files.clone())];
      (Some(Want::Rows { logs }), None)
    };
    Ok(TagsRead {
      files,
      want,
      texts,
      most: admission.left(),
      refused: None,
    })
  }

  /// The `.hgtags` of `store`'s heads read at once from what it answers now ([`Store::stored_now`], a whole answer's);
  /// refused where it answers only between steps.
  pub fn now(store: &impl Store, admission: &mut Admission) -> Result<TagsRead, Refusal> {
    let mut read = TagsRead::begin(store, admission)?;
    while let Some(want) = read.want.clone() {
      let given = store.stored_now(&want).ok_or(Refusal::Store {
        what: "a .hgtags text read from the changeset store between steps",
      })?;
      read.give(given);
    }
    Ok(read)
  }

  /// What the next read asks of the store, if anything.
  pub fn wants(&self) -> Option<&Want> {
    self.want.as_ref()
  }

  /// The store's answer to [`TagsRead::wants`].
  pub fn give(&mut self, given: Given) {
    match given {
      Given::Rows(rows) => self.give_rows(rows),
      Given::Texts(texts) => self.give_texts(texts),
      Given::TextsPast => self.texts_past(),
      Given::Chunks(_) => self.fail(Refusal::Store {
        what: "a .hgtags read answered with chunks",
      }),
    }
  }

  /// The rows of the `.hgtags` revisions: their texts asked next, within the room; refused where one has none.
  pub fn give_rows(&mut self, rows: Vec<Vec<Option<StoredRevision>>>) {
    self.want = None;
    let log = rows.into_iter().next().unwrap_or_default();
    let identities: Option<Vec<[u8; 32]>> = (log.len() == self.files.len())
      .then(|| log.into_iter().map(|row| row.map(|row| row.text)).collect())
      .flatten();
    match identities {
      Some(texts) => {
        self.want = Some(Want::Texts {
          texts,
          most: self.most,
        });
      }
      None => self.fail(Refusal::Store {
        what: "a .hgtags revision the changeset store keeps no row of",
      }),
    }
  }

  /// The texts of the `.hgtags` revisions, each with its metadata block left out.
  pub fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    self.want = None;
    if texts.len() != self.files.len() {
      return self.fail(Refusal::Store {
        what: "a .hgtags text the changeset store gave short",
      });
    }
    let mut read = Vec::with_capacity(texts.len());
    for text in texts {
      match crate::filerev::split(&text) {
        Ok(split) => read.push(split.bytes.to_vec()),
        Err(_) => return self.fail(Refusal::Store { what: "tags" }),
      }
    }
    self.texts = Some(read);
  }

  /// The texts hold more than the room the reading began in: the lookup is refused past it.
  pub fn texts_past(&mut self) {
    self.fail(Refusal::PastAdmission(crate::PastAdmission {
      asked: self.most.saturating_add(1),
      left: self.most,
    }));
  }

  /// Ends the read refused.
  fn fail(&mut self, refused: Refusal) {
    self.want = None;
    self.refused = Some(refused);
  }

  /// The texts read, oldest head's first, charged to `admission`; refused where the read failed or is not done.
  fn texts(&self, admission: &mut Admission) -> Result<&[Vec<u8>], Refusal> {
    if let Some(refused) = self.refused {
      return Err(refused);
    }
    let texts = self.texts.as_deref().ok_or(Refusal::Store {
      what: "a .hgtags text not read from the changeset store",
    })?;
    let len = texts.iter().fold(0_u64, |total, text| {
      total.saturating_add(crate::read::to_u64(text.len()))
    });
    admission.charge(len)?;
    Ok(texts)
  }
}

impl crate::render::StoreWants for TagsRead {
  fn wants(&self) -> Option<&Want> {
    TagsRead::wants(self)
  }

  fn give_rows(&mut self, rows: Vec<Vec<Option<StoredRevision>>>) {
    TagsRead::give_rows(self, rows);
  }

  fn give_chunks(&mut self, chunks: Vec<Vec<u8>>) {
    self.give(Given::Chunks(chunks));
  }

  fn give_texts(&mut self, texts: Vec<Vec<u8>>) {
    TagsRead::give_texts(self, texts);
  }

  fn texts_past(&mut self) {
    TagsRead::texts_past(self);
  }

  fn reads_tags(&self) -> bool {
    true
  }
}

impl TagsRead {
  /// Whether `self` read the same `.hgtags` revisions `other` begins to: the heads' tags unchanged between them, so a
  /// read made earlier answers a lookup now.
  pub fn same_files(&self, other: &TagsRead) -> bool {
    self.files == other.files
  }
}

/// Whether resolving `key` over `repo` reaches the tag namespace (module docs), so its `.hgtags` must be read first.
pub fn needs_tags(repo: &impl Repository, key: &[u8]) -> bool {
  before_tags(repo, key).is_none()
}

/// Writes `lookup`'s answer for `key` (module docs), the heads' `.hgtags` read in `tags` where the name reaches them.
pub fn lookup(
  repo: &impl Repository,
  tags: Option<&TagsRead>,
  key: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  match resolve(repo, tags, key, admission)? {
    Resolved::Node(node) => {
      out.extend_from_slice(FOUND);
      encode_hex(&node, out);
    }
    Resolved::Ambiguous(prefix) => {
      out.extend_from_slice(NOT_FOUND);
      out.extend_from_slice(CHANGELOG);
      out.extend_from_slice(&prefix);
      out.extend_from_slice(AMBIGUOUS);
    }
    Resolved::Unknown => {
      out.extend_from_slice(NOT_FOUND);
      out.extend_from_slice(UNKNOWN.0);
      out.extend_from_slice(key);
      out.extend_from_slice(UNKNOWN.1);
    }
  }
  out.push(b'\n');
  Ok(())
}

/// The node `name` names in the served view in 7.2's order (module docs): none for a name 7.2 does not know, or a
/// prefix more than one node begins. An owner's verb that names a changeset by a Mercurial name (A-575's `--from`)
/// resolves it here, as a client's `lookup` does.
pub fn resolve_name(
  repo: &impl Repository,
  tags: Option<&TagsRead>,
  name: &[u8],
  admission: &mut Admission,
) -> Result<Option<Node>, Refusal> {
  Ok(match resolve(repo, tags, name, admission)? {
    Resolved::Node(node) => Some(node),
    Resolved::Ambiguous(_) | Resolved::Unknown => None,
  })
}

/// `key` resolved in 7.2's order (module docs), the heads' `.hgtags` read in `tags`; refused where the name reaches the
/// tags and none were read.
fn resolve(
  repo: &impl Repository,
  tags: Option<&TagsRead>,
  key: &[u8],
  admission: &mut Admission,
) -> Result<Resolved, Refusal> {
  if let Some(resolved) = before_tags(repo, key) {
    return resolved;
  }
  let tags = tags.ok_or(Refusal::Store {
    what: "a .hgtags text not read from the changeset store",
  })?;
  if let Some(tagged) = tagged(repo, tags, key, admission)? {
    return Ok(tagged);
  }
  if let Some(rev) = branch_tip(repo, key, admission)? {
    return node_of(repo, rev).map(Resolved::Node);
  }
  prefixed(repo, key.strip_prefix(b"x").unwrap_or(key))
}

/// `key` resolved in 7.2's order up to the tags (module docs): `.`, `tip`, `null`, a revision number, a full node held
/// and a bookmark; none where it names none of those.
fn before_tags(repo: &impl Repository, key: &[u8]) -> Option<Result<Resolved, Refusal>> {
  let count = repo.count();
  match key {
    b"." | b"null" => return Some(Ok(Resolved::Node(NULL_NODE))),
    b"tip" => return Some(tip(repo, count).map(Resolved::Node)),
    _ => {}
  }
  if let Some(rev) = revision_number(key, count) {
    return Some(node_of(repo, rev).map(Resolved::Node));
  }
  if let Ok(Some(node)) = hex_node(key)
    && (node == NULL_NODE || repo.rev(&node).is_some())
  {
    return Some(Ok(Resolved::Node(node)));
  }
  repo
    .bookmarks()
    .find(|(name, node)| *name == key && repo.rev(node).is_some())
    .map(|(_, node)| Ok(Resolved::Node(node)))
}

/// What global tag `name` names (module docs): the `.hgtags` of each head of the view, oldest head first, each file
/// revision once (7.2's `_filterfnodes`), as `tags` read them, folded by [`Tag`]; a node the view does not serve is
/// unknown. None where no tag has the name, or its word is the null node.
fn tagged(
  repo: &impl Repository,
  tags: &TagsRead,
  name: &[u8],
  admission: &mut Admission,
) -> Result<Option<Resolved>, Refusal> {
  let mut tag = Tag::new(name);
  for text in tags.texts(admission)? {
    tag.read(text);
  }
  Ok(tag.node().map(|node| {
    Node::try_from(node)
      .ok()
      .filter(|node| repo.rev(node).is_some())
      .map_or(Resolved::Unknown, Resolved::Node)
  }))
}

/// The served view's tip, the null node when it holds none.
fn tip(repo: &impl Repository, count: Rev) -> Result<Node, Refusal> {
  match count.checked_sub(1) {
    Some(rev) => node_of(repo, rev),
    None => Ok(NULL_NODE),
  }
}

/// `key` as a revision number, as 7.2's `revsymbol` reads one: written as `%d` writes it, a negative one counted from
/// the end, within the view.
fn revision_number(key: &[u8], count: Rev) -> Option<Rev> {
  let (negative, digits) = match key.strip_prefix(b"-") {
    Some(digits) => (true, digits),
    None => (false, key),
  };
  let canonical = !digits.is_empty()
    && digits.iter().all(u8::is_ascii_digit)
    && (digits == b"0" || !digits.starts_with(b"0"))
    && !(negative && digits == b"0");
  if !canonical {
    return None;
  }
  let value: u64 = core::str::from_utf8(digits).ok()?.parse().ok()?;
  let rev = if negative {
    u64::from(count).checked_sub(value)?
  } else {
    value
  };
  Rev::try_from(rev).ok().filter(|rev| *rev < count)
}

/// The tip of the branch named `name`: its last open head, else its last head; `None` for a branch the view lacks.
fn branch_tip(
  repo: &impl Repository,
  name: &[u8],
  admission: &mut Admission,
) -> Result<Option<Rev>, Refusal> {
  // A branch with a revision has a head (its highest revision has no descendant on it), so the heads alone tell a
  // branch's name from another: no walk of the history asks whether it names one (goal 93).
  let heads: Vec<Rev> = branch_head_revs(repo, admission)?
    .into_iter()
    .filter(|rev| repo.branch(*rev) == name)
    .collect();
  let open = heads.iter().rev().find(|rev| !repo.closes(**rev));
  Ok(open.or(heads.last()).copied())
}

/// Format: a node's length in hexadecimal digits, two to a byte.
const NODE_HEX_LEN: usize = NULL_NODE.len() * 2;

/// The node `prefix` (hexadecimal digits, in either case) begins, among the view's nodes and the null node.
fn prefixed(repo: &impl Repository, prefix: &[u8]) -> Result<Resolved, Refusal> {
  if prefix.len() > NODE_HEX_LEN || !prefix.iter().all(u8::is_ascii_hexdigit) {
    return Ok(Resolved::Unknown);
  }
  let lowered = prefix.to_ascii_lowercase();
  // Read by node where the view can (goal 93): the prefix's whole bytes and its odd digit's high four bits, two
  // matches at most (two settle it as ambiguous), the null node beside them.
  let digit = |byte: u8| {
    char::from(byte)
      .to_digit(16)
      .and_then(|value| u8::try_from(value).ok())
  };
  let pairs = lowered.chunks_exact(2);
  let nibble = pairs.remainder().first().copied().and_then(digit);
  let bytes: Option<Vec<u8>> = pairs
    .map(|pair| Some(digit(*pair.first()?)? << 4 | digit(*pair.get(1)?)?))
    .collect();
  if let Some(bytes) = bytes
    && let Some(nodes) = repo.changesets_prefixed((&bytes, nibble), 2)
  {
    let null = lowered.iter().all(|byte| *byte == b'0');
    let mut matched = null.then_some(NULL_NODE).into_iter().chain(nodes);
    return Ok(match (matched.next(), matched.next()) {
      (None, _) => Resolved::Unknown,
      (Some(node), None) => Resolved::Node(node),
      (Some(_), Some(_)) => Resolved::Ambiguous(prefix.to_vec()),
    });
  }
  let mut found = None;
  let mut hex = Vec::with_capacity(NODE_HEX_LEN);
  let candidates =
    core::iter::once(Ok(NULL_NODE)).chain((0..repo.count()).map(|rev| node_of(repo, rev)));
  for node in candidates {
    let node = node?;
    hex.clear();
    encode_hex(&node, &mut hex);
    if hex.starts_with(&lowered) {
      if found.is_some() {
        return Ok(Resolved::Ambiguous(prefix.to_vec()));
      }
      found = Some(node);
    }
  }
  Ok(found.map_or(Resolved::Unknown, Resolved::Node))
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
