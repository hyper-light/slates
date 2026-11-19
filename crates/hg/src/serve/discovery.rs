//! Discovery's answers (A-619; §4.17.3 "Pull and clone" item 1): `heads`, `known`, `between`, `branches` and
//! `branchmap`, each written as 7.2's `wireprotov1server.py` writes it, over the served view a [`Repository`] presents.
//!
//! A stock client discovers what it lacks with `heads` and batches of `known` (7.2's `setdiscovery.py`: its own heads
//! asked first, then samples of what is still undecided, heads and their exponentially distant ancestors and the
//! roots' descendants, until nothing is). The legacy `between` and `branches` serve older clients' tree discovery, and
//! `between` with the null pair is every SSH session's handshake probe. Every answer is read off the served view at
//! one cut: its heads newest first, the null node for an empty repository, as `localrepo.heads` reverses
//! `headrevs()`; a branch's heads oldest first with closed heads kept, as `branchmap.branchheads(closed=True)`, and
//! the branches by name, the order 7.2's branch cache takes once written.

use super::{Refusal, Repository, Rev, RevSet, node_list, node_of, parents_of};
use crate::quote::{self, PATH_SAFE};
use crate::read::to_u64;
use crate::{Admission, NULL_NODE, Node, pushkey};

/// Format: the separator of a node list's items (`wireprototypes.encodelist`), and of `between`'s pairs.
const SPACE: u8 = b' ';

/// Format: the separator of a `between` pair's two nodes.
const PAIR: u8 = b'-';

/// Format: the end of an answer's line.
const NEWLINE: u8 = b'\n';

/// Format: `known`'s answer for a node the served view holds, and for one it does not.
const KNOWN: [u8; 2] = [b'0', b'1'];

/// Appends `heads`'s answer (`wireprotov1server.heads`): the served view's heads, newest first, as hexadecimal nodes
/// joined by spaces, then a newline; an empty repository answers the null node, as 7.2's does.
pub fn heads(
  repo: &impl Repository,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let heads = head_revs(repo, admission)?;
  if heads.is_empty() {
    pushkey::encode_hex(&NULL_NODE, out);
  }
  for (position, rev) in heads.iter().rev().enumerate() {
    if position > 0 {
      out.push(SPACE);
    }
    pushkey::encode_hex(&node_of(repo, *rev)?, out);
  }
  out.push(NEWLINE);
  Ok(())
}

/// The served view's heads, oldest first: the revisions no other revision names as a parent.
pub fn head_revs(repo: &impl Repository, admission: &mut Admission) -> Result<Vec<Rev>, Refusal> {
  let count = repo.count();
  let mut parents = RevSet::new(count, admission)?;
  for rev in 0..count {
    for parent in parents_of(repo, rev)?.into_iter().flatten() {
      parents.insert(parent);
    }
  }
  let heads = (0..count).filter(|rev| !parents.contains(*rev));
  admission.charge_items::<Rev>(heads.clone().count())?;
  Ok(heads.collect())
}

/// Appends `known`'s answer (`wireprotov1server.known`, `localrepo.known`): for each node of the space-separated list
/// `nodes`, `1` where the served view holds it and `0` where it does not; the null node, which 7.2 always holds, is `1`.
pub fn known(
  repo: &impl Repository,
  nodes: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let nodes = node_list(nodes, SPACE, admission)?;
  for node in &nodes {
    let held = *node == NULL_NODE || repo.rev(node).is_some();
    out.extend(KNOWN.get(usize::from(held)));
  }
  Ok(())
}

/// Appends `between`'s answer (`wireprotov1server.between`, `localrepo.between`): for each space-separated pair
/// `top-bottom`, a line of the first-parent ancestors of `top` at distances 1, 2, 4, 8 and on, before `bottom` or the
/// root is reached, as hexadecimal nodes joined by spaces. A top the served view does not hold is refused, as 7.2's
/// lookup of its parents fails; the bottom is compared, not looked up.
pub fn between(
  repo: &impl Repository,
  pairs: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  if pairs.is_empty() {
    return Ok(());
  }
  for (at, pair) in pairs.split(|byte| *byte == SPACE).enumerate() {
    let nodes = node_list(pair, PAIR, admission)?;
    let [top, bottom] =
      <[Node; 2]>::try_from(nodes).map_err(|_| Refusal::Pair { at: to_u64(at) })?;
    between_pair(repo, top, bottom, out)?;
    out.push(NEWLINE);
  }
  Ok(())
}

/// One pair's line: walks first parents from `top`, writing the revision met at each power-of-two distance.
fn between_pair(
  repo: &impl Repository,
  top: Node,
  bottom: Node,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let mut current = top;
  let mut distance: u64 = 0;
  let mut next_written: u64 = 1;
  let mut written = false;
  while current != bottom && current != NULL_NODE {
    let rev = repo
      .rev(&current)
      .ok_or(Refusal::UnknownNode { node: current })?;
    if distance == next_written {
      if written {
        out.push(SPACE);
      }
      pushkey::encode_hex(&current, out);
      written = true;
      next_written = next_written.saturating_mul(2);
    }
    current = match parents_of(repo, rev)? {
      [Some(first), _] => node_of(repo, first)?,
      [None, _] => NULL_NODE,
    };
    distance = distance.saturating_add(1);
  }
  Ok(())
}

/// Appends `branches`'s answer (`wireprotov1server.branches`, `localrepo.branches`): for each node of the
/// space-separated list `nodes` (the served view's tip when it is empty), a line of four hexadecimal nodes: the node,
/// the last of its first-parent run before a merge or a root, and that one's two parents. A node the served view does
/// not hold is refused, as 7.2's lookup of its parents fails.
pub fn branches(
  repo: &impl Repository,
  nodes: &[u8],
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let mut nodes = node_list(nodes, SPACE, admission)?;
  if nodes.is_empty() {
    admission.charge_items::<Node>(1)?;
    nodes.push(match repo.count().checked_sub(1) {
      Some(tip) => node_of(repo, tip)?,
      None => NULL_NODE,
    });
  }
  for top in nodes {
    let run = first_parent_run(repo, top)?;
    for (position, node) in run.iter().enumerate() {
      if position > 0 {
        out.push(SPACE);
      }
      pushkey::encode_hex(node, out);
    }
    out.push(NEWLINE);
  }
  Ok(())
}

/// `top`, the last revision of its first-parent run (where a revision has a second parent or no first one), and that
/// revision's parents, the null node for each that is missing.
fn first_parent_run(repo: &impl Repository, top: Node) -> Result<[Node; 4], Refusal> {
  if top == NULL_NODE {
    return Ok([NULL_NODE; 4]);
  }
  let mut rev = repo.rev(&top).ok_or(Refusal::UnknownNode { node: top })?;
  loop {
    match parents_of(repo, rev)? {
      [Some(first), None] => rev = first,
      [first, second] => {
        let node =
          |parent: Option<Rev>| parent.map_or(Ok(NULL_NODE), |parent| node_of(repo, parent));
        return Ok([top, node_of(repo, rev)?, node(first)?, node(second)?]);
      }
    }
  }
}

/// Appends `branchmap`'s answer (`wireprotov1server.branchmap`): a line for each named branch of the served view, by
/// name, of its name quoted as `urlreq.quote` quotes it and its heads (closed ones too) oldest first, as hexadecimal
/// nodes, the lines joined by newlines. A revision is its branch's head when no child of it is on its branch.
pub fn branchmap(
  repo: &impl Repository,
  admission: &mut Admission,
  out: &mut Vec<u8>,
) -> Result<(), Refusal> {
  let mut heads = branch_head_revs(repo, admission)?;
  heads.sort_unstable_by(|left, right| {
    repo
      .branch(*left)
      .cmp(repo.branch(*right))
      .then(left.cmp(right))
  });
  let mut previous: Option<&[u8]> = None;
  for rev in heads {
    let branch = repo.branch(rev);
    if previous == Some(branch) {
      out.push(SPACE);
    } else {
      if previous.is_some() {
        out.push(NEWLINE);
      }
      quote::quote(branch, PATH_SAFE, out);
      out.push(SPACE);
      previous = Some(branch);
    }
    pushkey::encode_hex(&node_of(repo, rev)?, out);
  }
  Ok(())
}

/// Every branch's heads, oldest first: the revisions no child on their own branch names as a parent.
pub fn branch_head_revs(
  repo: &impl Repository,
  admission: &mut Admission,
) -> Result<Vec<Rev>, Refusal> {
  let count = repo.count();
  let mut continued = RevSet::new(count, admission)?;
  for rev in 0..count {
    for parent in parents_of(repo, rev)?.into_iter().flatten() {
      if repo.branch(parent) == repo.branch(rev) {
        continued.insert(parent);
      }
    }
  }
  let heads = (0..count).filter(|rev| !continued.contains(*rev));
  admission.charge_items::<Rev>(heads.clone().count())?;
  Ok(heads.collect())
}
