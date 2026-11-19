//! A `sparse:oid` filter's blob, resolved and read by the service (A-640 part 4; git v2.50.1's `list-objects-filter.c`
//! `filter_sparse_oid__init`, `object-file.c` `read_object_with_reference` and `dir.c` `add_patterns_from_blob_to_list`,
//! the name resolved by the `revision` module; held to stock git's own answers in `fixtures/fetch-sparse-*` and
//! `fixtures/v0/sparse-*`).
//!
//! **What names the blob**: `<blob-ish>` read as `get_oid_with_context` reads it in a repository with no work tree,
//! which a served one is (the `revision` module): an object's full or abbreviated name, `git describe`'s, or a ref by
//! git's patterns, alone or before `:<path>`; an index path names nothing. A form git reads that this server does not
//! resolve yet is refused by kind ([`Refusal::SparseForm`]): a parent or an ancestor, a peel or a reflog, or a search
//! of commit messages (`:/<text>`). What git writes to its error stream while it reads the name (a warning of a ref
//! name that names something else too; an abbreviation's candidates) is relayed as git's server relays it, before the
//! pack or before the lines it dies with (`Read::said`).
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

use super::revision::{Named, Resolver};
use super::{Refusal, Served};
use crate::Admission;
use crate::negotiation::{Filter, Graph, Patterns, sparse};
use crate::object_name::{Kind, ObjectId};
use crate::store::StoreGraph;

/// What a request's `sparse:oid` filter brought: the patterns the negotiation walks by and, when the blob was not
/// resolved or read, why `pack-objects` dies as it begins. A dying blob brings no patterns, so the negotiation still
/// makes the sections before the pack, walking by none.
#[derive(Debug, Default)]
pub(super) struct Read {
  /// The patterns: `None` with no `sparse:oid` filter.
  pub(super) patterns: Option<Patterns>,
  /// Why `pack-objects` dies, when it does.
  pub(super) dies: Option<Refusal>,
  /// What git wrote to its error stream while it read the filter's name, a write each, in order.
  pub(super) said: Vec<Vec<u8>>,
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
  let Some(Filter::SparseOid(name)) = filter else {
    return Ok(Read::default());
  };
  let mut resolver = Resolver::new(graph, served);
  let named = resolver
    .blob_ish(name, admission)
    .map_err(Refusal::PastAdmission)?;
  let said = resolver.said();
  let refusal = match named {
    Named::Object(id) => match patterns_of(graph, id, admission)? {
      Ok(patterns) => {
        return Ok(Read {
          patterns: Some(patterns),
          dies: None,
          said,
        });
      }
      Err(refusal) => refusal,
    },
    Named::Nothing => Refusal::SparseBlob { name: name.clone() },
    Named::Form => Refusal::SparseForm { name: name.clone() },
    Named::Relative => Refusal::SparseRelative,
  };
  Ok(Read {
    patterns: Some(Patterns::default()),
    dies: Some(refusal),
    said,
  })
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
    .object_bytes(&id, Kind::Blob, admission)
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
