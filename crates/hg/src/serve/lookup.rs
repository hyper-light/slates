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
//! **Apart from 7.2.** `.` is the null node: a served repository has no working directory, so no parent; and it has no
//! local tags (`.hg/localtags`), so the global ones are all it holds.

use std::collections::HashSet;

use super::discovery::{branch_head_revs, head_revs};
use super::{Refusal, Repository, Rev, node_of};
use crate::pushkey::{encode_hex, hex_node};
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

/// Writes `lookup`'s answer for `key` (module docs).
pub fn lookup(
  repo: &impl Repository,
  key: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  match resolve(repo, key, admission)? {
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
  name: &[u8],
  admission: &mut Admission,
) -> Result<Option<Node>, Refusal> {
  Ok(match resolve(repo, name, admission)? {
    Resolved::Node(node) => Some(node),
    Resolved::Ambiguous(_) | Resolved::Unknown => None,
  })
}

/// `key` resolved in 7.2's order (module docs).
fn resolve(
  repo: &impl Repository,
  key: &[u8],
  admission: &mut Admission,
) -> Result<Resolved, Refusal> {
  let count = repo.count();
  match key {
    b"." | b"null" => return Ok(Resolved::Node(NULL_NODE)),
    b"tip" => return tip(repo, count).map(Resolved::Node),
    _ => {}
  }
  if let Some(rev) = revision_number(key, count) {
    return node_of(repo, rev).map(Resolved::Node);
  }
  if let Ok(Some(node)) = hex_node(key)
    && (node == NULL_NODE || repo.rev(&node).is_some())
  {
    return Ok(Resolved::Node(node));
  }
  if let Some((_, node)) = repo
    .bookmarks()
    .find(|(name, node)| *name == key && repo.rev(node).is_some())
  {
    return Ok(Resolved::Node(node));
  }
  if let Some(tagged) = tagged(repo, key, admission)? {
    return Ok(tagged);
  }
  if let Some(rev) = branch_tip(repo, key, admission)? {
    return node_of(repo, rev).map(Resolved::Node);
  }
  prefixed(repo, key.strip_prefix(b"x").unwrap_or(key))
}

/// What global tag `name` names (module docs): the `.hgtags` of each head of the view, oldest head first, each file
/// revision once (7.2's `_filterfnodes`), folded by [`Tag`]; a node the view does not serve is unknown. None where no
/// tag has the name, or its word is the null node.
fn tagged(
  repo: &impl Repository,
  name: &[u8],
  admission: &mut Admission,
) -> Result<Option<Resolved>, Refusal> {
  let heads = head_revs(repo, admission)?;
  admission.charge_items::<Node>(heads.len())?;
  let mut read: HashSet<Node> = HashSet::with_capacity(heads.len());
  let mut tag = Tag::new(name);
  for head in heads {
    let Some(file) = repo.tags_file(head, admission)? else {
      continue;
    };
    if read.insert(file) {
      tag.read(&repo.tags_text(&file, admission)?);
    }
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
  if !(0..repo.count()).any(|rev| repo.branch(rev) == name) {
    return Ok(None);
  }
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
