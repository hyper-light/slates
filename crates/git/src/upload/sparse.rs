//! A `sparse:oid` filter's blob, resolved and read by the service (A-640 part 4; git v2.50.1's `list-objects-filter.c`
//! `filter_sparse_oid__init`, `object-name.c` `get_oid_with_context_1`, `get_oid_1`, `get_oid_basic`,
//! `get_describe_name` and `resolve_relative_path`, `tree-walk.c` `get_tree_entry` and `find_tree_entry`,
//! `object-file.c` `read_object_with_reference`, and `dir.c` `add_patterns_from_blob_to_list`; held to stock git's own
//! answers in `fixtures/fetch-sparse-*` and `fixtures/v0/sparse-*`).
//!
//! **What names the blob**, `<blob-ish>` read as `get_oid_with_context_1` reads it in a repository with no work tree,
//! which a served one is:
//! - a revision alone: an object's full name in the repository's format, either case, held or not; or a ref by git's
//!   patterns (`refs::dwim`), `HEAD` and `@` naming the default branch;
//! - `:<path>` or `:<stage>:<path>`, a path in the index: nothing, as a served repository has no index;
//! - `<rev>:<path>`, split at the first `:` outside a `@{...}` or `^{...}`: the revision as above, peeled through tags
//!   and a commit to its tree, then the path walked as `find_tree_entry` walks it (`Tree::find`): a submodule's
//!   commit an entry like any other, a trailing `/` naming the directory, an empty path the tree itself.
//!
//! A path beginning `./` or `../` after a revision that resolves, or after a `:`, is refused as git refuses it with no
//! work tree. A form git reads that this server does not resolve is refused by kind ([`Refusal::SparseForm`]): a
//! parent or an ancestor (a revision whose last byte but its digits is `~` or `^`), a peel or a reflog (ending `}`
//! after `^{` or `@{`), an abbreviated object name, `git describe`'s name, or a search of commit messages (`:/<text>`).
//!
//! **Reading it.** The blob's bytes are charged to the request's admission before they are read, then its patterns
//! ([`Patterns::read`]); a blob past git's limit is refused unread when its length alone is past it.
//!
//! **git's refusals**, which `pack-objects` dies with as it begins, so the client meets them after the sections before
//! the pack, on band 2 (the `answer` module): a name that names nothing, "unable to access sparse blob in
//! '<blob-ish>'"; a relative path, "relative path syntax can't be used outside working tree"; an object that is no blob
//! the store holds, or a blob past git's limit (after git's warning of it), "unable to parse sparse filter data in
//! <id>". An answer that carries no pack never meets them, as git's never does.
//!
//! **Not git's.** Protocol v2 reads the blob in every round its filter rides in, since the negotiation walks by its
//! patterns in the round that sends the pack, so a blob past the request's admission is refused in a round git answers
//! with acknowledgments alone (the fetch fails either way; protocol v0 reads it only when the pack is due, as git).
//! git warns, on band 2, of a ref name two of its patterns find; this server takes the first, as git does, unwarned.

use super::{Refusal, Served, refs};
use crate::Admission;
use crate::negotiation::{Filter, Graph, Patterns, sparse};
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::refname;
use crate::store::StoreGraph;

/// Format: the byte between a revision and its path, and before an index path.
const COLON: u8 = b':';

/// Format: what a search of commit messages begins with (`:/<text>`).
const SEARCH: &[u8] = b":/";

/// Format: the stages an index path may name (`:<stage>:<path>`).
const STAGES: core::ops::RangeInclusive<u8> = b'0'..=b'3';

/// Format: what a relative path begins with (`resolve_relative_path`).
const RELATIVE: [&[u8]; 2] = [b"./", b"../"];

/// Format: the bytes before a `{` that open a braced suffix, within which a `:` splits nothing.
const BRACED: &[u8] = b"@^";

/// Format: a braced suffix's opening and closing bytes.
const BRACES: (u8, u8) = (b'{', b'}');

/// Format: the bytes that, last but a revision's digits, name a parent or an ancestor (`get_oid_1`).
const ANCESTRY: &[u8] = b"~^";

/// Format: the pairs whose suffix, closed by `}`, peels a revision or reads its reflog.
const SUFFIXES: [&[u8]; 2] = [b"^{", b"@{"];

/// Format: the shortest abbreviated object name git looks up (`MINIMUM_ABBREV`).
const MINIMUM_ABBREV: usize = 4;

/// Format: what `git describe` writes before its abbreviated name, after its count.
const DESCRIBE: &[u8] = b"-g";

/// Format: the revision git reads as `HEAD` (`interpret_empty_at`).
const AT: &[u8] = b"@";

/// Format: `HEAD`'s name.
const HEAD: &[u8] = b"HEAD";

/// What a request's `sparse:oid` filter brought: the patterns the negotiation walks by and, when the blob was not
/// resolved or read, why `pack-objects` dies as it begins. A dying blob brings no patterns, so the negotiation still
/// makes the sections before the pack, walking by none.
#[derive(Debug, Default)]
pub(super) struct Read {
  /// The patterns: `None` with no `sparse:oid` filter.
  pub(super) patterns: Option<Patterns>,
  /// Why `pack-objects` dies, when it does.
  pub(super) dies: Option<Refusal>,
}

/// What `filter` brings when it is `sparse:oid` alone (a combination holding one the negotiation refuses): its blob
/// resolved and read within `admission`. Refused only when the blob is past `admission`; a blob git's `pack-objects`
/// dies of is answered as it dies (`Read::dies`).
pub(super) fn read(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  filter: Option<&Filter>,
  admission: &mut Admission,
) -> Result<Read, Refusal> {
  let _ = (graph, served, filter, admission);
  Ok(Read::default())
}

/// The patterns of the blob `id` (`add_patterns_from_blob_to_list`), its bytes and patterns charged to `admission`:
/// refused (the outer `Err`) past `admission`, and refused as git's `pack-objects` dies (the inner `Err`) when `id`
/// names no blob the store holds, or one past git's limit, decided by its length before it is read where that suffices.
fn patterns_of(
  graph: &StoreGraph<'_>,
  id: ObjectId,
  admission: &mut Admission,
) -> Result<Result<Patterns, Refusal>, Refusal> {
  let unparsed = Err(Refusal::SparseData { id });
  let Some(node) = graph.node(&id) else {
    return Ok(unparsed);
  };
  if graph.kind(node) != Kind::Blob {
    return Ok(unparsed);
  }
  if graph.size(node) > sparse::MOST_BYTES {
    return Ok(Err(Refusal::SparseTooLarge { id }));
  }
  let Some(bytes) = graph
    .blob_bytes(&id, admission)
    .map_err(Refusal::PastAdmission)?
  else {
    return Ok(unparsed);
  };
  match Patterns::read(&bytes, admission) {
    Ok(patterns) => Ok(Ok(patterns)),
    Err(sparse::Refused::TooLarge { .. }) => Ok(Err(Refusal::SparseTooLarge { id })),
    Err(sparse::Refused::PastAdmission(past)) => Err(Refusal::PastAdmission(past)),
  }
}

/// What a `<blob-ish>` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Named {
  /// An object, held or not.
  Object(ObjectId),
  /// Nothing: `get_oid_with_context` finds no object.
  Nothing,
  /// A form of git's revision syntax this server does not resolve.
  Form,
  /// A relative path, which needs a work tree.
  Relative,
}

/// What `name` names (`get_oid_with_context_1`): the whole name as a revision first; then an index path; then a
/// revision and the path after its `:`.
fn named(graph: &StoreGraph<'_>, served: &Served<'_>, name: &[u8]) -> Named {
  if let Some(id) = revision(graph, served, name) {
    return Named::Object(id);
  }
  if let Some(index_path) = name.strip_prefix(&[COLON]) {
    return in_index(name, index_path);
  }
  let format = served.repository.format();
  let Some((rev, path)) = split(name) else {
    return unresolved(name, format);
  };
  let Some(id) = revision(graph, served, rev) else {
    return unresolved(rev, format);
  };
  if is_relative(path) {
    return Named::Relative;
  }
  tree_of(graph, &id)
    .and_then(|tree| walked(graph, tree, path))
    .map_or(Named::Nothing, Named::Object)
}

/// What `:<index path>` names in a repository with no index: a search of commit messages is a form this server does
/// not resolve; a relative path needs a work tree; any other path names nothing. `name` is the whole name.
fn in_index(name: &[u8], index_path: &[u8]) -> Named {
  if name.len() > SEARCH.len() && name.starts_with(SEARCH) {
    return Named::Form;
  }
  let path = match index_path {
    [stage, COLON, rest @ ..] if STAGES.contains(stage) => rest,
    _ => index_path,
  };
  if is_relative(path) {
    Named::Relative
  } else {
    Named::Nothing
  }
}

/// Whether `path` is relative, as `resolve_relative_path` reads one.
fn is_relative(path: &[u8]) -> bool {
  RELATIVE.iter().any(|prefix| path.starts_with(prefix))
}

/// `name` split at its first `:` outside a `@{...}` or `^{...}`: the revision before it and the path after it.
fn split(name: &[u8]) -> Option<(&[u8], &[u8])> {
  let (open, close) = BRACES;
  let mut depth = 0_usize;
  let mut at = 0_usize;
  while let Some(&byte) = name.get(at) {
    let next = at.saturating_add(1);
    if BRACED.contains(&byte) && name.get(next) == Some(&open) {
      depth = depth.saturating_add(1);
      at = next;
    } else if depth > 0 && byte == close {
      depth = depth.saturating_sub(1);
    } else if depth == 0 && byte == COLON {
      return Some((name.get(..at)?, name.get(next..)?));
    }
    at = at.saturating_add(1);
  }
  None
}

/// The object a revision names, as this server resolves one (`get_oid_basic`): an object's full name in the
/// repository's format, either case, held or not; or what the ref the first of git's patterns finds names, `@` read as
/// `HEAD`.
fn revision(graph: &StoreGraph<'_>, served: &Served<'_>, rev: &[u8]) -> Option<ObjectId> {
  if let Ok(id) = ObjectId::from_hex(served.repository.format(), rev) {
    return Some(id);
  }
  let rev = if rev == AT { HEAD } else { rev };
  refs::dwim(graph, served, rev).map(|node| graph.id(node))
}

/// What a revision this server did not resolve is: a form of git's revision syntax it does not resolve, which git
/// may, or nothing, which git finds nothing for either.
fn unresolved(rev: &[u8], format: ObjectFormat) -> Named {
  let ancestry = rev
    .iter()
    .rev()
    .find(|byte| !byte.is_ascii_digit())
    .is_some_and(|byte| ANCESTRY.contains(byte));
  let braced = rev.last() == Some(&BRACES.1)
    && SUFFIXES
      .iter()
      .any(|pair| rev.windows(pair.len()).any(|window| window == *pair));
  if ancestry || braced || is_abbreviated(rev, format) || is_described(rev, format) {
    Named::Form
  } else {
    Named::Nothing
  }
}

/// Whether `rev` may be an abbreviated object name (`get_short_oid`): hexadecimal digits, at least git's shortest and
/// fewer than a full name's.
fn is_abbreviated(rev: &[u8], format: ObjectFormat) -> bool {
  (MINIMUM_ABBREV..format.hex_len()).contains(&rev.len()) && rev.iter().all(u8::is_ascii_hexdigit)
}

/// Whether `rev` is a name `git describe` writes (`get_describe_name`, `ref_and_count_parts_valid`):
/// `<ref>-<count>-g<abbreviated name>`, the count hexadecimal digits, the ref a one-level name git accepts and the name
/// no longer than a full one, found at the last `-g` that has them before it.
fn is_described(rev: &[u8], format: ObjectFormat) -> bool {
  let digits = rev
    .iter()
    .rev()
    .take_while(|byte| byte.is_ascii_hexdigit())
    .count();
  let before = rev.len().saturating_sub(digits);
  let Some(head) = rev
    .get(..before)
    .and_then(|head| head.strip_suffix(DESCRIBE))
  else {
    return false;
  };
  (MINIMUM_ABBREV..=format.hex_len()).contains(&digits) && counted_ref(head)
}

/// Whether `head` is `<ref>-<count>`, the count one hexadecimal digit or more and the ref a one-level name
/// `check_refname_format` passes (its components' rule, and not `@` alone).
fn counted_ref(head: &[u8]) -> bool {
  let count = head
    .iter()
    .rev()
    .take_while(|byte| byte.is_ascii_hexdigit())
    .count();
  let Some(name) = head
    .get(..head.len().saturating_sub(count))
    .and_then(|rest| rest.strip_suffix(b"-"))
  else {
    return false;
  };
  count > 0 && name != AT && refname::tag_name_is_valid(name)
}

/// The tree `id` names, peeled as `read_object_with_reference` peels for a tree: the tree itself, a commit's root tree,
/// or what a chain of tags ends at. `None` for a blob, an object the store does not hold, and a chain of tags that
/// comes back on itself, which a store whose names are hashes cannot hold.
fn tree_of(graph: &StoreGraph<'_>, id: &ObjectId) -> Option<u64> {
  let node = graph.node(id)?;
  let node = match graph.kind(node) {
    Kind::Tag => refs::peeled_node(graph, node)?,
    _ => node,
  };
  match graph.kind(node) {
    Kind::Tree => Some(node),
    Kind::Commit => graph.commit(node).map(|row| row.tree),
    Kind::Tag | Kind::Blob => None,
  }
}

/// The object at `path` below the tree at `tree` (`get_tree_entry`): the tree itself for an empty path; otherwise
/// each entry git's walk finds (`Tree::find`), descending into a directory's tree, peeled as git peels it, while
/// path is left below it. `None` when an entry is not found, or its tree is not one the store holds. Each step takes a
/// component of `path`, so the walk ends within its length.
fn walked(graph: &StoreGraph<'_>, tree: u64, path: &[u8]) -> Option<ObjectId> {
  let mut tree = tree;
  let mut left = path;
  while !left.is_empty() {
    let (entry, below) = graph.tree_entry(tree, left)?;
    if below.is_empty() {
      return Some(entry);
    }
    tree = tree_of(graph, &entry)?;
    left = below;
  }
  Some(graph.id(tree))
}
