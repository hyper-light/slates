//! Works over works (§4.17.1 "Works over works" and "Chains of works", shared piece 5; §4.16 "Position mapping" and
//! "Rebase"; A-659).
//!
//! **What it is.** A work may be based on another work's post-state, not only on a version of the green: a review
//! request built on another (a pull request's stack, a topic's changesets), or an agent's next work on its last one
//! before that lands. Such a work is an [`Upper`]: its ops document as the deriver composed it against its lower work's
//! post-state, as a work volume cloned from the lower composes its journal. A stack is a work based on a version, its
//! bottom, and the uppers over it, each over the post-state of the one below.
//!
//! **Landing a stack** ([`Green::land_stack`]) lands its works in order, each all or none, in one call: the bottom is
//! judged as any work is; each upper, once the work below it landed, is restated over the version that work landed as
//! ([`Green::restate`]) and judged as a work based on that version. A work that does not land leaves every work above
//! it unjudged ([`Landed::LowerRefused`]), so no client round trip is needed to learn it. A stack landed again is
//! answered each landed work's version: an upper restated over a version is judged under an identity derived from its
//! own and that version, so the green's acceptance record finds it.
//!
//! **Restating an upper.** The version a lower landed as differs from its post-state by what the green's other works
//! changed between the lower's base and its landing, which the verdict mapped the lower through. Those changes are read
//! from the green path by path, for the paths the upper names (the deltas and histories it keeps, at the cost of each
//! path's own changes), and moved over each lower's operations by the rule the verdict mapped the lower with: an
//! operation ending at or before another's start shifts it, and the first starting at or after its end ends the walk
//! ([`crate::map`]). So the moved changes and the lower's landed operations come from one decision, and the landed
//! operations are checked against what the green recorded when the lower landed. The upper is then judged against the
//! moved changes as the verdict judges a work against the changes after its base: its ranges are mapped through them, a
//! range one meets is a conflict unless the upper's bytes are the new base's there, and an operation on a path one
//! removed, made or changed in the dimension it reads is judged as the verdict judges one changed after its base. What
//! landed after the version it is restated over is the verdict's, as for any work. The lower is never judged again, and
//! the upper's base already holds the lower's own changes, so they are never mapped: the design's "mapped through the
//! green's deltas except the lower work's own".
//!
//! **What a lower absorbed.** A change a lower made too (the verdict accepted it as identical) is in the upper's base
//! already, so the upper does not meet it: a same-span overwrite, and a metadata value the lower set. Any other change
//! a lower's operation met (an identical replacement of another shape, which the verdict accepted whole) moves by no
//! position mapping, so every operation of an upper on that file is a conflict: never a wrong accept.
//!
//! **What it refuses**, typed ([`RestateRefused`]): a work below that has not landed; a span of the green's history a
//! lower was mapped through that is folded below the checkpoint or fold floor; a copy in a stack (a restatement carries
//! none, as a rebase carries none, owed with the deriver's copy); and a lower whose landing the green recorded otherwise
//! than its moved operations, a guard that never fires on a correct engine.
//!
//! **A base given as a tree** ([`Green::restate_over_tree`]; §4.17.1 "A push is a work" item 6) is a base that is no
//! version: a commit inside an earlier push. What the green changed since it is read as whole-value differences between
//! the tree and the head at the paths the work names (`tree`, its module docs).

use std::collections::{BTreeMap, BTreeSet};

use super::{ConflictWindow, Green, Increment, Judgment, Resolved, node_exclusive_paths};
use crate::chain_entry::Applied;
use crate::ops_doc::OpsDoc;

mod judge;
mod levels;
mod moved;
mod tree;

use judge::Against;
use levels::Level;
use moved::Change;

pub use tree::{TreeBase, TreeNode};

#[cfg(test)]
mod handed_tests;
#[cfg(test)]
mod oracle;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tree_tests;

/// Format: the BLAKE3 key-derivation context an upper restated over a version is judged under (module docs), so its
/// identity is never a work volume's, a push's or a tree-based work's.
const UPPER_CONTEXT: &str = "slates 2026-11-19 an upper work restated over its lower's landing";

/// Format: the BLAKE3 key-derivation context a work over a tree, restated over a head, is judged under.
const TREE_CONTEXT: &str = "slates 2026-11-19 a work over a tree restated over a head";

/// A work based on another work's post-state (module docs): its ops document as the deriver composed it against the
/// lower work's post-state, and that document's post-state bytes. It names no version: its base is its lower's
/// post-state, and the version it is judged on is the one its lower lands as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upper {
  /// The work's identity (`blake3` of its declared work, as an increment's).
  pub id: [u8; 32],
  /// The canonical ops document, composed against the lower's post-state.
  pub doc: OpsDoc,
  /// The post-state bytes its content operations name.
  pub post_state: Vec<u8>,
  /// The evidence references the submitter attached, opaque here (§4.16 `Increment.evidence`).
  pub evidence: Vec<[u8; 32]>,
}

/// What landing one work of a stack answered ([`Green::land_stack`]), in the stack's order: the bottom first.
#[allow(
  clippy::large_enum_variant,
  reason = "an accepted work is the common outcome: boxing it would allocate per landed work, to shrink the rare refusals"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landed {
  /// The work landed.
  Accepted {
    /// The version it landed as, now or before.
    version: u64,
    /// For an upper, the work as it was judged and committed: restated over the version the work below it landed as,
    /// which is what the green's chain records of its version. None for the bottom, committed as given.
    restated: Option<Increment>,
    /// What the verdict resolved it to (`crate::chain_entry`); none when it was accepted before.
    applied: Option<Vec<Applied>>,
  },
  /// It meets a change it did not see: the windows, nothing of it applied.
  Conflict {
    /// The conflicting windows.
    windows: Vec<ConflictWindow>,
  },
  /// The bottom's base is below the version the green's chain was checkpointed at (as
  /// [`super::Outcome::BaseUnavailable`]).
  BaseUnavailable {
    /// The bottom's base.
    base: u64,
    /// The oldest base the green still judges.
    floor: u64,
  },
  /// It could not be restated over the version below it, typed.
  Refused(RestateRefused),
  /// A copy it declares is refused before any verdict (as [`super::Outcome::CopyRefused`]).
  CopyRefused {
    /// The copy's destination.
    path: String,
    /// Why.
    case: super::CopyRefusal,
  },
  /// A named run of the work is not read yet (as [`super::Outcome::ContentUnread`]).
  ContentUnread {
    /// The run's object.
    identity: crate::named::Identity,
  },
  /// Never judged: a work below it did not land.
  LowerRefused {
    /// The place in the stack of the first work below it that did not land (the bottom is 0).
    lower: usize,
  },
}

/// An upper restated over the version the work below it landed as ([`Green::restate`]), or over a tree's head
/// ([`Green::restate_over_tree`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restated {
  /// The upper as a work based on that version, its operations in that version's coordinates: what the green judges.
  Over(Increment),
  /// It meets a change it did not see: the windows, nothing changed.
  Conflict {
    /// The conflicting windows.
    windows: Vec<ConflictWindow>,
  },
  /// It cannot be restated, typed.
  Refused(RestateRefused),
}

/// Why an upper cannot be restated (module docs, "What it refuses").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestateRefused {
  /// A work below it has not landed.
  NotLanded {
    /// Its place in the stack (the bottom is 0).
    below: usize,
  },
  /// The green's history a lower was mapped through is folded below what it reconstructs.
  Folded {
    /// The oldest version the green's history is kept from.
    floor: u64,
  },
  /// A work of the stack copies a tree, which a restatement does not carry (owed with the deriver's copy).
  Copy {
    /// The copy's destination.
    path: String,
  },
  /// The green recorded a lower's landing on a path otherwise than its moved operations: never met on a correct
  /// engine, and refused rather than restated by a guess.
  Diverged {
    /// The path.
    path: String,
    /// The version the lower landed as.
    version: u64,
  },
}

impl Green {
  /// Lands a stack in order, each work all or none (module docs): `bottom`, a work based on a version of the green, then
  /// each of `uppers`, each based on the post-state of the work before it. Answers each work in the stack's order. A
  /// work below that does not land leaves every work above it unjudged; a stack landed again is answered each landed
  /// work's version and commits nothing new.
  pub fn land_stack(&mut self, bottom: &Increment, uppers: &[Upper]) -> Vec<Landed> {
    let mut landed = Vec::with_capacity(uppers.len().saturating_add(1));
    let mut first_refused = None;
    let outcome = self.land_one(Landing::Bottom(bottom));
    if !matches!(outcome, Landed::Accepted { .. }) {
      first_refused = Some(0);
    }
    landed.push(outcome);
    for (index, upper) in uppers.iter().enumerate() {
      if let Some(lower) = first_refused {
        landed.push(Landed::LowerRefused { lower });
        continue;
      }
      let lowers = uppers.get(..index).unwrap_or_default();
      let outcome = match self.restate((bottom, lowers), upper) {
        Restated::Over(increment) => self.land_one(Landing::Upper(increment)),
        Restated::Conflict { windows } => Landed::Conflict { windows },
        Restated::Refused(why) => Landed::Refused(why),
      };
      if !matches!(outcome, Landed::Accepted { .. }) {
        first_refused = Some(index.saturating_add(1));
      }
      landed.push(outcome);
    }
    landed
  }

  /// Restates `upper` over the version the work below it landed as (module docs): `stack` is the works below it, the
  /// bottom and the uppers in order, each of which must have landed. The upper as a work based on that version, or the
  /// windows of the changes it meets, or why it cannot be restated.
  pub fn restate(&self, stack: (&Increment, &[Upper]), upper: &Upper) -> Restated {
    match self.restated(stack, upper) {
      Ok(Ok(work)) => Restated::Over(work),
      Ok(Err(windows)) => Restated::Conflict { windows },
      Err(why) => Restated::Refused(why),
    }
  }

  /// [`Green::restate`]: the upper restated, or its windows; or why it cannot be.
  fn restated(
    &self,
    (bottom, lowers): (&Increment, &[Upper]),
    upper: &Upper,
  ) -> Result<Result<Increment, Vec<ConflictWindow>>, RestateRefused> {
    let levels = self.levels(bottom, lowers)?;
    let at = levels.last().map_or(bottom.base, Level::landed);
    let mut work = Increment {
      id: restated_identity(UPPER_CONTEXT, &upper.id, at),
      base: at,
      doc: upper.doc.clone(),
      post_state: upper.post_state.clone(),
      evidence: upper.evidence.clone(),
      named: Default::default(),
    };
    let resolved = Green::resolve(&work);
    if let Some(path) = resolved.copies.keys().next() {
      return Err(RestateRefused::Copy { path: path.clone() });
    }
    let mut changes = BTreeMap::new();
    for path in named(&resolved, &work.doc) {
      let change = self.change_at(&levels, &path)?;
      changes.insert(path, change);
    }
    let landed: BTreeSet<u64> = levels.iter().map(Level::landed).collect();
    let against = Against {
      changes: &changes,
      at,
      unchanged: Change::default(),
      since: (bottom.base, &landed),
    };
    let judged = self.judge_upper((&work, &resolved), &against);
    Ok(judged.map(|ops| {
      work.doc.ops = ops;
      work
    }))
  }

  /// The works below an upper (`levels`): each one's operations, the span of the green it was mapped through and the
  /// version it landed as; refused when one has not landed, declares a copy, or was mapped through history the green
  /// no longer holds.
  fn levels(&self, bottom: &Increment, lowers: &[Upper]) -> Result<Vec<Level>, RestateRefused> {
    let floor = self.folded_below.max(self.deltas_from);
    if bottom.base < floor {
      return Err(RestateRefused::Folded { floor });
    }
    let mut landed = self
      .accepted
      .get(&bottom.id)
      .copied()
      .ok_or(RestateRefused::NotLanded { below: 0 })?;
    let mut levels = Vec::with_capacity(lowers.len().saturating_add(1));
    levels.push(Level::of(&bottom.doc, (bottom.base, landed))?);
    for (index, lower) in lowers.iter().enumerate() {
      let id = restated_identity(UPPER_CONTEXT, &lower.id, landed);
      let below = index.saturating_add(1);
      let next = self
        .accepted
        .get(&id)
        .copied()
        .ok_or(RestateRefused::NotLanded { below })?;
      levels.push(Level::of(&lower.doc, (landed, next))?);
      landed = next;
    }
    Ok(levels)
  }

  /// Judges and commits a work of a stack at the head: the version it landed as, or why it did not.
  fn land_one(&mut self, landing: Landing<'_>) -> Landed {
    match self.judge(landing.work()) {
      Judgment::Accepted(judged) => {
        let (version, applied) = self.commit_held(judged);
        Landed::Accepted {
          version,
          restated: landing.restated(),
          applied: Some(applied),
        }
      }
      Judgment::AcceptedBefore { version } => Landed::Accepted {
        version,
        restated: landing.restated(),
        applied: None,
      },
      Judgment::Conflict { windows } => Landed::Conflict { windows },
      Judgment::BaseUnavailable { base, floor } => Landed::BaseUnavailable { base, floor },
      Judgment::CopyRefused { path, case } => Landed::CopyRefused { path, case },
      Judgment::ContentUnread { identity } => Landed::ContentUnread { identity },
    }
  }
}

/// A work of a stack as it is landed ([`Green::land_one`]): the bottom as given, or an upper's restatement, which its
/// answer carries for the chain.
enum Landing<'a> {
  /// The bottom.
  Bottom(&'a Increment),
  /// An upper, restated.
  Upper(Increment),
}

impl Landing<'_> {
  /// The work judged.
  fn work(&self) -> &Increment {
    match self {
      Landing::Bottom(work) => work,
      Landing::Upper(work) => work,
    }
  }

  /// The restatement its answer carries: an upper's.
  fn restated(self) -> Option<Increment> {
    match self {
      Landing::Bottom(_) => None,
      Landing::Upper(work) => Some(work),
    }
  }
}

/// The identity a work restated over `base` is judged under: its own identity and the version, under `context` (module
/// docs), so a stack landed again finds each restated work's acceptance, and one work restated over two versions is two
/// works.
fn restated_identity(context: &str, id: &[u8; 32], base: u64) -> [u8; 32] {
  let mut hasher = blake3::Hasher::new_derive_key(context);
  hasher.update(id);
  hasher.update(&base.to_le_bytes());
  *hasher.finalize().as_bytes()
}

/// Every path an upper's verdict reads (`judge`): the paths its operations change, the sources its renames move, the
/// nodes its hard links name, and a node-exclusive operation's paths.
fn named(resolved: &Resolved, doc: &OpsDoc) -> BTreeSet<String> {
  let mut paths: BTreeSet<String> = resolved.content.keys().cloned().collect();
  for (path, source) in &resolved.renames {
    paths.insert(path.clone());
    paths.insert(source.clone());
  }
  for (path, target) in &resolved.hardlinks {
    paths.insert(path.clone());
    paths.extend(resolved.link_node(target).map(|(node, _)| node.to_owned()));
  }
  paths.extend(resolved.unlinks.iter().cloned());
  paths.extend(resolved.rmdirs.iter().cloned());
  paths.extend(resolved.modes.keys().cloned());
  paths.extend(resolved.symlinks.keys().cloned());
  paths.extend(resolved.specials.keys().cloned());
  paths.extend(resolved.xattr_sets.iter().map(|(path, ..)| path.clone()));
  paths.extend(resolved.xattr_removes.iter().map(|(path, _)| path.clone()));
  for op in doc.ops.iter().filter(|op| op.is_node_exclusive()) {
    for (index, _) in node_exclusive_paths(op) {
      paths.extend(doc.paths.path(index).map(str::to_owned));
    }
  }
  paths
}
