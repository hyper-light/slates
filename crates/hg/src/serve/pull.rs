//! What a pull reads beside discovery (A-619; §4.17.3 "Pull and clone" items 1 to 3): `listkeys`'s answers, which
//! changesets a `getbundle` sends, and the phase heads and bookmarks that travel with them, each as 7.2 computes it.
//!
//! **The changesets** (7.2's `exchange._computeoutgoing` and `discovery.outgoing`): the client's common nodes the
//! served view does not hold are dropped, the null revision standing for none; the heads are the ones asked for, the
//! served view's own when none are, and one it does not hold is refused; what the client lacks is every ancestor of a
//! head that is no ancestor of a common revision, oldest first. One walk finds it: down the revisions from the newest
//! named, each marked as reached from a head, from a common revision or both, those reached from a head alone being
//! missing, until no revision reached from a head alone is left below (7.2's `missingancestors`); its marks are a byte
//! for each revision of the view, charged first.
//!
//! **Phases** (`_getbundlephasespart`): a publishing repository sends every head public. Another sends each head in its
//! own phase, and with draft heads also the public heads among what the draft heads hold and the public ones do not
//! (`heads(only(draft, public) and public())`), so a client learns where public ends; each phase's nodes sorted.
//!
//! **Bookmarks and `listkeys`.** The `bookmarks` part carries every bookmark naming a changeset the served view holds,
//! by name, but a divergent one (an `@` before its name's end), as 7.2 lists them to a peer (`listbinbookmarks`:
//! `hasnode(v) and not isdivergent(k)`; A-690). `listkeys` answers `namespaces` with 7.2's four, `bookmarks` with each
//! such bookmark's hexadecimal node, and `phases` with the served view's draft roots (each `1`, draft's number) and
//! `publishing` `True` on a publishing repository (`listphases`), sorted by key; any other namespace answers nothing.
//! `obsolete`'s markers, which 7.2 lists in base85 chunks, come with the markers' exchange.

use super::discovery::head_revs;
use super::{Refusal, Repository, Rev, node_list, node_of, parents_of};
use crate::bookmarks::{Bookmark, divergent};
use crate::phases::{Phase, PhaseHead};
use crate::read::to_u64;
use crate::{Admission, NODE_LEN, Node, pushkey};

/// Format: the pushkey namespaces 7.2 lists (`pushkey._namespaces`), in the order `listkeys` sorts them.
pub const NAMESPACES: [&[u8]; 4] = [b"bookmarks", b"namespaces", b"obsolete", b"phases"];

/// Format: the `phases` key that tells a client the repository publishes, and its value (`listphases`).
const PUBLISHING: (&[u8], &[u8]) = (b"publishing", b"True");

/// Format: a node list's separator in `getbundle`'s `heads` and `common` (`wireprototypes.decodelist`).
const SPACE: u8 = b' ';

/// Format: a node's hexadecimal digits.
const HEX_LEN: usize = NODE_LEN * 2;

/// Format: a revision marked as reached from a head the walk started at.
const FROM_HEAD: u8 = 1;

/// Format: a revision marked as reached from a common revision.
const FROM_COMMON: u8 = 1 << 1;

/// Format: a revision found missing.
const MISSING: u8 = 1 << 2;

/// What a `getbundle` sends: its heads, the client's common revisions, and the changesets the client lacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
  /// The heads asked for, or the served view's heads when none were, oldest first.
  pub heads: Vec<Rev>,
  /// The client's common nodes the served view holds, oldest first; none stands for the null revision.
  pub common: Vec<Rev>,
  /// The changesets the client lacks: every ancestor of a head (the head too) that is no ancestor of a common
  /// revision, oldest first.
  pub missing: Vec<Rev>,
}

/// Appends `listkeys`'s answer for `namespace` (`wireprotov1server.listkeys`: the namespace's keys sorted, encoded as
/// pushkey's `listkeys` lines).
pub fn listkeys(
  repo: &impl Repository,
  namespace: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let draft = Phase::Draft.number().to_string();
  let mut hexes = Vec::new();
  let mut names: Vec<&[u8]> = Vec::new();
  match namespace {
    b"namespaces" => names.extend(NAMESPACES),
    b"bookmarks" => {
      let marks = bookmark_entries(repo, admission)?;
      hex_nodes(
        marks.iter().filter_map(|mark| mark.node),
        admission,
        &mut hexes,
      )?;
      names.extend(marks.iter().map(|mark| mark.name));
    }
    b"phases" => hex_nodes(
      draft_roots(repo, admission)?.into_iter(),
      admission,
      &mut hexes,
    )?,
    _ => {}
  }
  let (digits, _) = hexes.as_chunks::<HEX_LEN>();
  let mut entries: Vec<(&[u8], &[u8])> = Vec::new();
  admission.charge_items::<(&[u8], &[u8])>(names.len().max(digits.len()).saturating_add(1))?;
  match namespace {
    b"namespaces" => entries.extend(names.iter().map(|name| (*name, &b""[..]))),
    b"bookmarks" => entries.extend(names.iter().copied().zip(digits.iter().map(|hex| &hex[..]))),
    b"phases" => {
      entries.extend(digits.iter().map(|hex| (&hex[..], draft.as_bytes())));
      if repo.publishing() {
        entries.push(PUBLISHING);
      }
      entries.sort_unstable();
    }
    _ => {}
  }
  pushkey::encode_listkeys(&entries, out).map_err(|_| Refusal::Unencodable {
    what: "a listkeys key or value holding a tab or a line end",
  })
}

/// Writes each node's 40 hexadecimal digits one after another into `hexes`, charged first.
fn hex_nodes(
  nodes: impl Iterator<Item = Node> + Clone,
  admission: &mut Admission,
  hexes: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let len = nodes
    .clone()
    .count()
    .saturating_mul(NODE_LEN.saturating_mul(2));
  admission.charge(to_u64(len))?;
  hexes.reserve_exact(len);
  for node in nodes {
    pushkey::encode_hex(&node, hexes);
  }
  Ok(())
}

/// The served view's draft roots, sorted: its draft revisions none of whose parents is draft.
fn draft_roots(repo: &impl Repository, admission: &mut Admission) -> Result<Vec<Node>, Refusal> {
  let mut roots = Vec::new();
  let count = repo.count();
  let is_root = |rev: Rev| -> Result<bool, Refusal> {
    let draft_parent = parents_of(repo, rev)?
      .into_iter()
      .flatten()
      .any(|parent| repo.phase(parent) != Phase::Public);
    Ok(repo.phase(rev) != Phase::Public && !draft_parent)
  };
  let mut found = 0_usize;
  for rev in 0..count {
    found = found.saturating_add(usize::from(is_root(rev)?));
  }
  admission.charge_items::<Node>(found)?;
  roots.reserve_exact(found);
  for rev in 0..count {
    if is_root(rev)? {
      roots.push(node_of(repo, rev)?);
    }
  }
  roots.sort_unstable();
  Ok(roots)
}

/// Every bookmark naming a changeset the served view holds, but a divergent one, sorted by name (`listbinbookmarks`:
/// `hasnode(v) and not isdivergent(k)`), as the `bookmarks` part carries them and `listkeys` lists them.
pub fn bookmark_entries<'r>(
  repo: &'r impl Repository,
  admission: &mut Admission,
) -> Result<Vec<Bookmark<'r>>, Refusal> {
  let held = || {
    repo
      .bookmarks()
      .filter(|(name, node)| !divergent(name) && repo.rev(node).is_some())
  };
  admission.charge_items::<Bookmark<'_>>(held().count())?;
  let mut marks: Vec<Bookmark<'r>> = held()
    .map(|(name, node)| Bookmark {
      name,
      node: Some(node),
    })
    .collect();
  marks.sort_unstable_by(|left, right| left.name.cmp(right.name));
  Ok(marks)
}

/// `getbundle`'s choice of changesets for its space-separated `heads` and `common` node lists (7.2's
/// `_computeoutgoing`).
pub fn outgoing(
  repo: &impl Repository,
  heads: &[u8],
  common: &[u8],
  admission: &mut Admission,
) -> Result<Outgoing, Refusal> {
  let asked = node_list(heads, SPACE, admission)?;
  let heads = if asked.is_empty() {
    head_revs(repo, admission)?
  } else {
    admission.charge_items::<Rev>(asked.len())?;
    let mut revs = Vec::with_capacity(asked.len());
    for node in &asked {
      revs.push(repo.rev(node).ok_or(Refusal::UnknownNode { node: *node })?);
    }
    revs.sort_unstable();
    revs.dedup();
    revs
  };
  let named = node_list(common, SPACE, admission)?;
  admission.charge_items::<Rev>(named.len())?;
  let mut common: Vec<Rev> = named.iter().filter_map(|node| repo.rev(node)).collect();
  common.sort_unstable();
  common.dedup();
  let missing = missing(repo, &heads, &common, admission)?;
  Ok(Outgoing {
    heads,
    common,
    missing,
  })
}

/// The ancestors of `heads` (themselves included) that are no ancestor of `common`, oldest first: one walk down from
/// the newest revision named, stopping once no revision reached from a head alone is left below.
pub fn missing(
  repo: &impl Repository,
  heads: &[Rev],
  common: &[Rev],
  admission: &mut Admission,
) -> Result<Vec<Rev>, Refusal> {
  let count = repo.count();
  let Some(top) = heads.iter().chain(common).copied().max() else {
    return Ok(Vec::new());
  };
  if top >= count {
    return Err(Refusal::Graph { rev: top });
  }
  admission.charge_items::<u8>(usize::try_from(top).unwrap_or(usize::MAX).saturating_add(1))?;
  let mut marks = vec![0_u8; usize::try_from(top).unwrap_or(usize::MAX).saturating_add(1)];
  let mut live: u64 = 0;
  for (revs, mark) in [(heads, FROM_HEAD), (common, FROM_COMMON)] {
    for rev in revs {
      add_mark(&mut marks, *rev, mark, &mut live);
    }
  }
  let mut found: usize = 0;
  for rev in (0..=top).rev() {
    if live == 0 {
      break;
    }
    let Some(mark) = marks
      .get(usize::try_from(rev).unwrap_or(usize::MAX))
      .copied()
    else {
      continue;
    };
    if mark == 0 {
      continue;
    }
    let reached = if mark & FROM_COMMON != 0 {
      FROM_COMMON
    } else {
      live = live.saturating_sub(1);
      found = found.saturating_add(1);
      if let Some(slot) = marks.get_mut(usize::try_from(rev).unwrap_or(usize::MAX)) {
        *slot |= MISSING;
      }
      FROM_HEAD
    };
    for parent in parents_of(repo, rev)?.into_iter().flatten() {
      add_mark(&mut marks, parent, reached, &mut live);
    }
  }
  admission.charge_items::<Rev>(found)?;
  let mut missing = Vec::with_capacity(found);
  missing.extend((0..=top).filter(|rev| {
    marks
      .get(usize::try_from(*rev).unwrap_or(usize::MAX))
      .is_some_and(|mark| mark & MISSING != 0)
  }));
  Ok(missing)
}

/// Marks `rev` as reached by `mark`, keeping `live` the count of revisions reached from a head alone and not yet
/// walked.
fn add_mark(marks: &mut [u8], rev: Rev, mark: u8, live: &mut u64) {
  let Some(slot) = marks.get_mut(usize::try_from(rev).unwrap_or(usize::MAX)) else {
    return;
  };
  let head_alone = |marks: u8| marks & FROM_HEAD != 0 && marks & FROM_COMMON == 0;
  let before = head_alone(*slot);
  *slot |= mark;
  match (before, head_alone(*slot)) {
    (false, true) => *live = live.saturating_add(1),
    (true, false) => *live = live.saturating_sub(1),
    _ => {}
  }
}

/// The `phase-heads` part's entries for a `getbundle` of `heads` (7.2's `_getbundlephasespart`): every head public
/// on a publishing repository; otherwise each head in its phase, and with draft heads the public heads of what the
/// draft heads hold and the public heads do not; each phase's nodes sorted, public first.
pub fn phase_heads(
  repo: &impl Repository,
  heads: &[Rev],
  admission: &mut Admission,
) -> Result<Vec<PhaseHead>, Refusal> {
  let mut public: Vec<Rev> = Vec::new();
  let mut draft: Vec<Rev> = Vec::new();
  for head in heads {
    if repo.publishing() || repo.phase(*head) == Phase::Public {
      public.push(*head);
    } else {
      draft.push(*head);
    }
  }
  if !draft.is_empty() {
    let only = missing(repo, &draft, &public, admission)?;
    admission.charge_items::<(Rev, bool)>(only.len())?;
    let held: Vec<Rev> = only
      .iter()
      .copied()
      .filter(|rev| repo.phase(*rev) == Phase::Public)
      .collect();
    let mut parent_of_held = vec![false; held.len()];
    for rev in &held {
      for parent in parents_of(repo, *rev)?.into_iter().flatten() {
        if let Ok(at) = held.binary_search(&parent)
          && let Some(slot) = parent_of_held.get_mut(at)
        {
          *slot = true;
        }
      }
    }
    public.extend(
      held
        .iter()
        .zip(&parent_of_held)
        .filter(|(_, parent)| !**parent)
        .map(|(rev, _)| *rev),
    );
  }
  let mut entries = Vec::new();
  for (phase, revs) in [(Phase::Public, public), (Phase::Draft, draft)] {
    let mut nodes = Vec::with_capacity(revs.len());
    for rev in revs {
      nodes.push(node_of(repo, rev)?);
    }
    nodes.sort_unstable();
    nodes.dedup();
    admission.charge_items::<PhaseHead>(nodes.len())?;
    entries.extend(nodes.into_iter().map(|node| PhaseHead { phase, node }));
  }
  Ok(entries)
}
