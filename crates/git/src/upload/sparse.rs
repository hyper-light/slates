//! A `sparse:oid` filter's blob, resolved and read by the service (A-640 part 4; git's `filter_sparse_oid__init`,
//! `get_oid_with_context` with `GET_OID_BLOB`, `add_patterns_from_blob_to_list`; git v2.50.1, held to stock git's own
//! answers in `fixtures/fetch-sparse-*`).
//!
//! **What names the blob.** `<blob-ish>` is a full object name of the repository's format, taken as it is, held or not;
//! or `<rev>:<path>`, `<rev>` a full object name or a ref by git's patterns (`refs::revision`), peeled through tags to a
//! commit's tree or naming a tree, and `<path>` the names of the entries below it, joined by `/`; or `<rev>` alone. A
//! `<blob-ish>` of any other form git reads (an abbreviated name, `HEAD~1`, `@{...}`) is not resolved here, and is
//! answered as one that names nothing.
//!
//! **git's two refusals**, which `pack-objects` dies with as it begins, so the client meets them after the packfile
//! section's header, on band 2, then git's band-3 words, and no flush: a name that resolves to nothing, "unable to access
//! sparse blob in '<blob-ish>'"; and one that resolves to an object that is no blob the store holds, or to a blob past
//! git's limit, "unable to parse sparse filter data in <id>". An answer that carries no pack never reads the blob, as
//! git's never does: a round answered by acknowledgments alone is answered whatever its filter's blob.

use super::{Refusal, Served, refs};
use crate::Admission;
use crate::negotiation::{Filter, Graph, Patterns, sparse};
use crate::object_name::{Kind, ObjectId};
use crate::store::StoreGraph;

/// Format: the byte between a `<rev>` and its `<path>`.
const COLON: u8 = b':';

/// Format: the byte between a path's segments.
const SLASH: u8 = b'/';

/// What a request's `sparse:oid` filter reads.
pub(super) enum Sparse {
  /// The blob's patterns.
  Patterns(Patterns),
  /// Why git's `pack-objects` dies as it begins: a pack is answered with git's words for it.
  Dies(Refusal),
}

/// What `filter` reads when it is `sparse:oid` alone (a combination holding one the negotiation refuses): its blob
/// resolved and read within `admission`.
pub(super) fn read(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  filter: Option<&Filter>,
  admission: &mut Admission,
) -> Result<Option<Sparse>, Refusal> {
  let Some(Filter::SparseOid(name)) = filter else {
    return Ok(None);
  };
  let Some(id) = resolve(graph, served, name) else {
    return Ok(Some(Sparse::Dies(Refusal::SparseBlob {
      name: name.clone(),
    })));
  };
  let unparsed = Sparse::Dies(Refusal::SparseData { id });
  let past_limit = graph
    .node(&id)
    .is_some_and(|node| graph.kind(node) == Kind::Blob && graph.size(node) > sparse::MOST_BYTES);
  if past_limit {
    return Ok(Some(unparsed));
  }
  let Some(bytes) = graph
    .blob_bytes(&id, admission)
    .map_err(Refusal::PastAdmission)?
  else {
    return Ok(Some(unparsed));
  };
  match Patterns::read(&bytes, admission) {
    Ok(patterns) => Ok(Some(Sparse::Patterns(patterns))),
    Err(sparse::Refused::TooLarge { .. }) => Ok(Some(unparsed)),
    Err(sparse::Refused::PastAdmission(past)) => Err(Refusal::PastAdmission(past)),
  }
}

/// The object `name` names (`get_oid_with_context`): a full object name as it is; `<rev>:<path>` walked from what
/// `<rev>` names; `<rev>` alone. `None` when it names nothing this server resolves.
fn resolve(graph: &StoreGraph<'_>, served: &Served<'_>, name: &[u8]) -> Option<ObjectId> {
  if let Ok(id) = ObjectId::from_hex(served.repository.format(), name) {
    return Some(id);
  }
  let Some(at) = name.iter().position(|byte| *byte == COLON) else {
    return refs::revision(graph, served, name).map(|node| graph.id(node));
  };
  let rev = name.get(..at)?;
  let path = name.get(at.saturating_add(1)..)?;
  let tree = tree_of(graph, refs::revision(graph, served, rev)?)?;
  walk(graph, tree, path).map(|node| graph.id(node))
}

/// The tree `node` names: its own, a commit's root tree, or that of what a chain of tags ends at.
fn tree_of(graph: &StoreGraph<'_>, node: u64) -> Option<u64> {
  let mut current = node;
  loop {
    match graph.kind(current) {
      Kind::Tree => return Some(current),
      Kind::Commit => return graph.commit(current).map(|row| row.tree),
      Kind::Tag => current = graph.target(current)?,
      Kind::Blob => return None,
    }
  }
}

/// The object at `path` below `tree` (`get_tree_entry`): each segment an entry's name, the tree itself for an empty
/// path; `None` when a segment names no entry, or one that is no tree has path left below it.
fn walk(graph: &StoreGraph<'_>, tree: u64, path: &[u8]) -> Option<u64> {
  if path.is_empty() {
    return Some(tree);
  }
  let mut current = tree;
  for segment in path.split(|byte| *byte == SLASH) {
    if graph.kind(current) != Kind::Tree {
      return None;
    }
    current = graph
      .named_entries(current)
      .find(|(_, name)| name.as_slice() == segment)
      .map(|(node, _)| node)?;
  }
  Some(current)
}
