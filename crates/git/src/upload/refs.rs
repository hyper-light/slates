//! The refs, as upload-pack reads them (A-640 part 2; §4.17.2 "A fetch" item 1 and "The repository in git's terms" item
//! 6; `ls-refs.c` `ls_refs`, `send_ref` and `send_possibly_unborn_head`; `upload-pack.c` `parse_want_ref` and
//! `process_deepen_not`; `refs.c` `expand_ref` and `ref_rev_parse_rules`).
//!
//! **`ls-refs`.** `HEAD` comes first, where a prefix takes it, as the default branch's symref: its target named when the
//! client asked for `symrefs`, and, when that branch has no commit yet, listed as `unborn` only when the client asked
//! for both `unborn` and `symrefs`, as git lists it. Then every ref a prefix takes, in name order, each an annotated
//! tag's peeled id beside it when the client asked for `peel`, peeled through every tag it names in turn. The served
//! refs are in name order, so each prefix takes a run of them, found by a search: the prefixes are sorted and any a
//! shorter one begins is dropped, since the shorter one's run holds its own, and so no ref is listed twice and the list
//! stays in name order, as git's `refs_for_each_fullref_in_prefixes` lists. No prefix takes every ref. git stops
//! honouring prefixes past 65536 of them; this server honours all of them, the request's admission bounding how many.
//!
//! **Names a `fetch` gives.** A `want-ref` is a ref's full name, `HEAD` read through to the default branch
//! (`refs_read_ref`). A `deepen-not` is tried, as `expand_ref` tries it, as each of git's patterns (the name itself, then
//! under `refs/`, `refs/tags/`, `refs/heads/`, `refs/remotes/`, and `refs/remotes/<name>/HEAD`), and must name exactly
//! one ref. A ref whose green has no version names nothing, as no git ref does.

use core::cmp::Ordering;

use super::{Refusal, Served, charged};
use crate::Admission;
use crate::negotiation::Graph;
use crate::object_name::{Kind, ObjectId};
use crate::store::{NamedRef, StoreGraph};
use crate::v2::Line;
use crate::v2::ls_refs::{self, Argument, Attribute, Attributes, Ref, Target};

/// Format: `HEAD`'s name.
const HEAD: &[u8] = b"HEAD";

/// Format: the patterns git tries a short name as, in order, the name between each pair's halves (`refs.c`
/// `ref_rev_parse_rules`).
const REV_PARSE_RULES: [(&[u8], &[u8]); 6] = [
  (b"", b""),
  (b"refs/", b""),
  (b"refs/tags/", b""),
  (b"refs/heads/", b""),
  (b"refs/remotes/", b""),
  (b"refs/remotes/", b"/HEAD"),
];

/// Refused when `refs` are not in name order, each once: where the first out of place is.
pub(super) fn check_order(refs: &[NamedRef<'_>]) -> Result<(), Refusal> {
  let misplaced = refs
    .windows(2)
    .position(|pair| !matches!(pair, [earlier, later] if earlier.name < later.name));
  match misplaced {
    Some(at) => Err(Refusal::Refs {
      at: at.saturating_add(1),
    }),
    None => Ok(()),
  }
}

/// The object the ref named `name` names now, by position: its green's current head.
pub(super) fn named(graph: &StoreGraph<'_>, refs: &[NamedRef<'_>], name: &[u8]) -> Option<u64> {
  let at = refs.binary_search_by(|named| named.name.cmp(name)).ok()?;
  graph.head(refs.get(at)?.green)
}

/// The object a `want-ref` names (`parse_want_ref`): the ref of that full name, `HEAD` read through to the default
/// branch.
pub(super) fn wanted(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  name: &[u8],
) -> Result<ObjectId, Refusal> {
  let full = if name == HEAD { served.head } else { name };
  named(graph, served.refs, full)
    .map(|node| graph.id(node))
    .ok_or_else(|| Refusal::UnknownRef {
      name: name.to_vec(),
    })
}

/// The object a `deepen-not` names (`process_deepen_not`): `rev` tried as each of git's patterns, which must name
/// exactly one ref between them.
pub(super) fn deepen_not(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  rev: &[u8],
) -> Result<ObjectId, Refusal> {
  let mut found = REV_PARSE_RULES
    .iter()
    .filter_map(|&(before, after)| resolved(graph, served, [before, rev, after]));
  match (found.next(), found.next()) {
    (Some(node), None) => Ok(graph.id(node)),
    (None, _) => Err(Refusal::DeepenNotRef { rev: rev.to_vec() }),
    (Some(_), Some(_)) => Err(Refusal::DeepenNotAmbiguous { rev: rev.to_vec() }),
  }
}

/// The object a revision names, by position (A-640 part 4, a `sparse:oid` filter's `<rev>`): a full object name of the
/// repository's format, or the ref the first of git's patterns finds (`repo_dwim_ref`, which takes the first and only
/// warns of the rest), `HEAD` read through to the default branch. `None` for any other form git reads, which this
/// server does not.
pub(super) fn revision(graph: &StoreGraph<'_>, served: &Served<'_>, rev: &[u8]) -> Option<u64> {
  if let Ok(id) = ObjectId::from_hex(served.repository.format(), rev) {
    return graph.node(&id);
  }
  REV_PARSE_RULES
    .iter()
    .find_map(|&(before, after)| resolved(graph, served, [before, rev, after]))
}

/// What the ref whose name `parts` make up, in order, names: `HEAD` read through to the default branch.
fn resolved(graph: &StoreGraph<'_>, served: &Served<'_>, parts: [&[u8]; 3]) -> Option<u64> {
  if compare(HEAD, parts) == Ordering::Equal {
    return named(graph, served.refs, served.head);
  }
  let at = served
    .refs
    .binary_search_by(|named| compare(named.name, parts))
    .ok()?;
  graph.head(served.refs.get(at)?.green)
}

/// How `name` compares with the name `parts` make up, in order, byte by byte as names are ordered.
fn compare(name: &[u8], parts: [&[u8]; 3]) -> Ordering {
  name.iter().cmp(parts.iter().flat_map(|part| part.iter()))
}

/// What an `ls-refs` request asks for besides its prefixes.
#[derive(Clone, Copy, Debug, Default)]
struct Asked {
  /// `peel`.
  peel: bool,
  /// `symrefs`.
  symrefs: bool,
  /// `unborn`.
  unborn: bool,
}

/// The `ls-refs` answer to `arguments` (`ls_refs`): `HEAD` first where a prefix takes it, then each ref a prefix takes,
/// in name order, then the flush; the lines and the bytes charged to `admission` before they are made.
pub(super) fn ls_refs(
  graph: &StoreGraph<'_>,
  served: &Served<'_>,
  arguments: &[Line<Argument<'_>>],
  admission: &mut Admission,
) -> Result<Vec<u8>, Refusal> {
  let asked = Asked {
    peel: ls_refs::holds(arguments, Argument::Peel),
    symrefs: ls_refs::holds(arguments, Argument::Symrefs),
    unborn: ls_refs::holds(arguments, Argument::Unborn),
  };
  let prefixes = covering(arguments, admission)?;
  let lines = lines(graph, served, asked, &prefixes, admission)?;
  let format = served.repository.format();
  let (bytes, ()) = charged(admission, |out| {
    ls_refs::encode_answer(&lines, format, out).map_err(Refusal::Wire)
  })?;
  match graph.fault() {
    Some(fault) => Err(Refusal::Store(fault)),
    None => Ok(bytes),
  }
}

/// The prefixes that take every ref a prefix of `arguments` takes, each once, in order: a prefix a shorter one begins
/// is dropped, since the shorter one's run holds its own. No prefix is the empty one, which takes every ref.
fn covering<'a>(
  arguments: &[Line<Argument<'a>>],
  admission: &mut Admission,
) -> Result<Vec<&'a [u8]>, Refusal> {
  admission
    .charge_items::<&[u8]>(arguments.len().max(1))
    .map_err(Refusal::PastAdmission)?;
  let mut prefixes: Vec<&'a [u8]> = Vec::with_capacity(arguments.len().max(1));
  prefixes.extend(ls_refs::prefixes(arguments));
  if prefixes.is_empty() {
    prefixes.push(b"");
  }
  prefixes.sort_unstable();
  prefixes.dedup_by(|later, kept| later.starts_with(*kept));
  Ok(prefixes)
}

/// The refs `prefixes` take, in name order: each prefix's run, found by a search, since the refs are in name order.
fn taken<'r, 'n>(
  refs: &'r [NamedRef<'n>],
  prefixes: &'r [&'r [u8]],
) -> impl Iterator<Item = &'r NamedRef<'n>> + 'r {
  prefixes.iter().flat_map(move |prefix| {
    let start = refs.partition_point(|named| named.name < *prefix);
    refs
      .get(start..)
      .unwrap_or_default()
      .iter()
      .take_while(move |named| named.name.starts_with(prefix))
  })
}

/// The answer's lines: `HEAD`'s first where a prefix takes it, then each ref's, charged to `admission` before the list
/// is made.
fn lines<'s>(
  graph: &StoreGraph<'_>,
  served: &Served<'s>,
  asked: Asked,
  prefixes: &[&[u8]],
  admission: &mut Admission,
) -> Result<Vec<Line<Ref<'s>>>, Refusal> {
  let head = prefixes
    .iter()
    .any(|prefix| HEAD.starts_with(prefix))
    .then(|| head_line(graph, served, asked))
    .flatten();
  let count = taken(served.refs, prefixes)
    .count()
    .saturating_add(usize::from(head.is_some()));
  admission
    .charge_items::<Line<Ref<'s>>>(count)
    .map_err(Refusal::PastAdmission)?;
  let mut lines = Vec::with_capacity(count);
  lines.extend(head.map(Line::lf));
  for named in taken(served.refs, prefixes) {
    if let Some(node) = graph.head(named.green) {
      lines.push(Line::lf(ref_line(graph, named.name, node, asked.peel)));
    }
  }
  Ok(lines)
}

/// `HEAD`'s line (`send_possibly_unborn_head`): what the default branch names, its target where `symrefs` was asked;
/// or, for a branch with no commit yet, `unborn` where both `unborn` and `symrefs` were.
fn head_line<'s>(graph: &StoreGraph<'_>, served: &Served<'s>, asked: Asked) -> Option<Ref<'s>> {
  let target = Attribute::SymrefTarget(served.head);
  match named(graph, served.refs, served.head) {
    Some(node) => {
      let mut line = ref_line(graph, HEAD, node, asked.peel);
      if asked.symrefs {
        let peeled = line.attributes.peeled().map(Attribute::Peeled);
        line.attributes = [Some(target), peeled]
          .into_iter()
          .flatten()
          .try_fold(Attributes::default(), Attributes::with)?;
      }
      Some(line)
    }
    None => (asked.unborn && asked.symrefs).then(|| Ref {
      target: Target::Unborn,
      name: HEAD,
      attributes: Attributes::default().with(target).unwrap_or_default(),
    }),
  }
}

/// A ref's line: what it names, and, where `peel` was asked, what an annotated tag names, peeled.
fn ref_line<'n>(graph: &StoreGraph<'_>, name: &'n [u8], node: u64, peel: bool) -> Ref<'n> {
  let peeled = peel.then(|| peeled(graph, node)).flatten();
  Ref {
    target: Target::Id(graph.id(node)),
    name,
    attributes: peeled
      .and_then(|id| Attributes::default().with(Attribute::Peeled(id)))
      .unwrap_or_default(),
  }
}

/// What the annotated tag at `node` names, peeled through every tag it names in turn (`peel_iterated_oid`): `None` for an
/// object that is no tag, and for a chain of tags that comes back on itself, which a store whose names are hashes
/// cannot hold. Floyd's check (a second walker at twice the pace meets the first inside a loop) keeps the walk finite
/// without a list of what it passed.
pub(super) fn peeled(graph: &StoreGraph<'_>, node: u64) -> Option<ObjectId> {
  let mut slow = node;
  let mut fast = node;
  while graph.kind(fast) == Kind::Tag {
    fast = graph.target(fast)?;
    if graph.kind(fast) != Kind::Tag {
      break;
    }
    fast = graph.target(fast)?;
    slow = graph.target(slow)?;
    if slow == fast {
      return None;
    }
  }
  (fast != node).then(|| graph.id(fast))
}
