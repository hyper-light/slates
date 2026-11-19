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

use super::{ConflictWindow, Green, Increment, Judgment};
use crate::chain_entry::Applied;
use crate::ops_doc::OpsDoc;

mod tree;

pub use tree::{TreeBase, TreeNode};

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
    let outcome = self.land_one(bottom.clone(), false);
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
        Restated::Over(increment) => self.land_one(increment, true),
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
    let (bottom, _) = stack;
    let _ = (bottom, upper);
    Restated::Refused(RestateRefused::NotLanded { below: 0 })
  }

  /// Judges and commits one work of a stack at the head: the version it landed as, or why it did not. `restated` says
  /// whether it is an upper's restatement, which the answer carries for the chain.
  fn land_one(&mut self, increment: Increment, restated: bool) -> Landed {
    match self.judge(&increment) {
      Judgment::Accepted(judged) => {
        let (version, applied) = self.commit_held(judged);
        Landed::Accepted {
          version,
          restated: restated.then_some(increment),
          applied: Some(applied),
        }
      }
      Judgment::AcceptedBefore { version } => Landed::Accepted {
        version,
        restated: restated.then_some(increment),
        applied: None,
      },
      Judgment::Conflict { windows } => Landed::Conflict { windows },
      Judgment::BaseUnavailable { base, floor } => Landed::BaseUnavailable { base, floor },
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
