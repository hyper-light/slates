//! A copy from an older revision, made without the green's history (P7's finding (1); main's ruling of 2026-11-21
//! 10:55): what each path the copy's source changed since the copied revision must be set back to.
//!
//! **Why.** FSFS copies from any committed revision. The green's copy of an older version rebuilt the source's tree at
//! that version from the history it kept (`slates_merge`'s `CopyFix`), so the green kept every version a copy might
//! name: 440 MB for git-2000's 35 MB of history, and a copy refused `VersionFolded` once a tight budget folded it. Since
//! C2 stage B every text is readable through the records and the node's store, so a copy needs none of that history.
//!
//! **What.** The increment copies the source as it stands at the head, then sets back every path whose node at the
//! head is not the node of the copied revision ([`Restore`]): removed where the head holds a node the revision did not,
//! made where the revision held one the head does not, its text and property list set to the revision's where they
//! differ. The source's two trees are walked together from the copy's root, a directory whose node-revision is the
//! same in both skipped whole: the index bubbles every change up to the root (a directory's node-revision changes with
//! anything beneath it), so equal node-revisions are equal trees. The work is the paths the source changed since the
//! copied revision, not the source's size.
//!
//! **Bounded.** The walk is user-scaled, so it runs a slice at a time under the shard's step budget, a yield between
//! slices, holding the repository's landing turn so the records it read are the ones the revision lands on; each slice
//! is counted ([`SLICES`]). The texts it names are brought through the node's store before the commit's step, as a row's
//! are (`super::bring_stored`).
//!
//! **Not declared yet.** A node whose kind differs between the two revisions beneath the copy's root (a file where a
//! directory stood) is refused `Unsupported`, as the increment's other changes of kind are; and a row of the same
//! revision that adds or replaces a node the restore sets back.

use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_svn::commit::revision::StepRefusal;
use slates_svn::index::{ChangedPath, NodeKind, PropertyList, Text};
use slates_svn::migration::Repository;

use super::{Stood, Uncommitted, green_path};

/// Counter: a slice of a copy's restore walk (non-vacuity: a copy from an older revision walked its source). Format: a
/// counter name in the status report.
pub(crate) const SLICES: &str = "repository.svn_commit.restore_slices";

/// What a path beneath a copy from an older revision is set back to (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Restore {
  /// The head holds a node the copied revision did not: removed.
  Remove {
    /// Whether it is a directory.
    directory: bool,
  },
  /// The node as the copied revision held it, over what the copy from the head brought (`brought`; none where it
  /// brought nothing there).
  Set {
    /// What the copy from the head brought there.
    brought: Option<Stood>,
    /// Whether the node is a directory.
    directory: bool,
    /// Its text then, for a file; none for an empty one or a directory.
    text: Option<Text>,
    /// Its property list then.
    props: PropertyList,
  },
}

/// One copy's restore: whether its source stands at the head (the copy is made from there, else made whole from the
/// revision), and what each green path beneath its destination is set back to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CopyPlan {
  /// Whether the source stands at the head as the same kind of node.
  pub(super) from_head: bool,
  /// Each green path set back, with what to.
  pub(super) restores: Vec<(String, Restore)>,
}

/// Each copy of a revision from an older revision, by its row's path.
pub(crate) type CopyPlans = BTreeMap<String, CopyPlan>;

/// The texts `plans` set back: what the commit's step reads whole, brought through the node's store first.
pub(super) fn texts_of(plans: &CopyPlans) -> Vec<Text> {
  plans
    .values()
    .flat_map(|plan| plan.restores.iter())
    .filter_map(|(_, restore)| match restore {
      Restore::Set { text, .. } => *text,
      Restore::Remove { .. } => None,
    })
    .collect()
}

/// One copy's walk under way: the source's revision and the head, and the paths still to compare (a source path and
/// its destination's).
struct Walk {
  /// The copied revision.
  revision: u64,
  /// The head the copy is made from.
  head: u64,
  /// The paths still to compare.
  stack: Vec<(String, String)>,
  /// The plan so far.
  plan: CopyPlan,
}

/// Plans every copy `changes` make from a revision older than repository `repository`'s youngest (module docs), a
/// slice at a time under the shard's step budget with a yield between, each slice counted. The caller holds the
/// repository's landing turn. None where the shard's state is gone; refused where the records do not hold together, or
/// `Unsupported` for a change of kind beneath a copy.
pub(super) async fn plan(
  repository: RepositoryId,
  changes: &[ChangedPath],
) -> Option<Result<CopyPlans, Uncommitted>> {
  let mut plans = CopyPlans::new();
  for change in changes {
    let Some(source) = &change.copy_from else {
      continue;
    };
    let head = crate::state::with_state(|state| {
      state
        .vcs_stores
        .subversion_records(repository)
        .map(Repository::youngest)
    })?;
    let Some(head) = head else {
      return Some(Err(Uncommitted::NotHeld));
    };
    if source.revision >= head {
      continue;
    }
    let mut walk = Walk {
      revision: source.revision,
      head,
      stack: vec![(source.path.clone(), change.path.clone())],
      plan: CopyPlan::default(),
    };
    let mut root = true;
    loop {
      let stepped = crate::state::with_state(|state| {
        state.count(SLICES, 1);
        let budget = slates_rt::futures::step_budget_ns().unwrap_or(0);
        let deadline = slates_rt::futures::now_ns().saturating_add(budget);
        let records = state
          .vcs_stores
          .subversion_records(repository)
          .ok_or(Uncommitted::NotHeld)?;
        step(records, (&mut walk, &mut root), deadline)
      })?;
      match stepped {
        Ok(true) => break,
        Ok(false) => slates_rt::futures::yield_now().await,
        Err(refused) => return Some(Err(refused)),
      }
    }
    plans.insert(change.path.clone(), walk.plan);
  }
  Some(Ok(plans))
}

/// The child `name` of the path `parent`.
fn child(parent: &str, name: &str) -> String {
  if parent == "/" {
    format!("/{name}")
  } else {
    format!("{parent}/{name}")
  }
}

/// What a node stands as, for the increment.
fn stood_of(kind: NodeKind, text: Option<Text>, props: PropertyList) -> Stood {
  Stood {
    directory: kind == NodeKind::Directory,
    len: text.map_or(0, |text| text.size),
    props: props != PropertyList::Empty,
  }
}

/// A node as the walk compares it: its node-revision, kind, text and property list.
type Seen = (
  slates_svn::index::NodeRevisionId,
  NodeKind,
  Option<Text>,
  PropertyList,
);

/// Compares paths of `walk` until it is whole (true) or `deadline_ns` passes (false), at least one. `root` says the
/// next path is the copy's own, which sets whether the copy is made from the head.
fn step(
  records: &Repository,
  (walk, root): (&mut Walk, &mut bool),
  deadline_ns: u64,
) -> Result<bool, Uncommitted> {
  let index = records.index();
  let refused = |refusal| Uncommitted::Step(StepRefusal::Index(refusal));
  let seen = |path: &str, revision: u64| -> Result<Option<Seen>, Uncommitted> {
    let node = index.node(path, revision).map_err(refused)?;
    Ok(node.map(|node| (node.id(), node.kind(), node.text(), node.properties())))
  };
  let names = |path: &str, revision: u64| -> Result<Vec<String>, Uncommitted> {
    Ok(
      index
        .children(path, revision)
        .map_err(refused)?
        .unwrap_or_default(),
    )
  };
  loop {
    let Some((source, dest)) = walk.stack.pop() else {
      return Ok(true);
    };
    let old = seen(&source, walk.revision)?;
    let mut new = seen(&source, walk.head)?;
    let at_root = core::mem::replace(root, false);
    if at_root {
      walk.plan.from_head = matches!((&old, &new), (Some(old), Some(new)) if old.1 == new.1);
      if !walk.plan.from_head {
        new = None;
      }
    }
    let to = green_path(&dest).to_owned();
    match (old, new) {
      (None, None) => {}
      (Some(old), Some(new)) if old.0 == new.0 => {}
      (Some(old), Some(new)) if old.1 != new.1 => {
        return Err(Uncommitted::Unsupported { path: dest });
      }
      (Some(old), Some(new)) => {
        let differs =
          old.2.map(|text| text.identity) != new.2.map(|text| text.identity) || old.3 != new.3;
        let directory = old.1 == NodeKind::Directory;
        if differs {
          walk.plan.restores.push((
            to,
            Restore::Set {
              brought: Some(stood_of(new.1, new.2, new.3)),
              directory,
              text: old.2,
              props: old.3,
            },
          ));
        }
        if directory {
          let mut all = names(&source, walk.revision)?;
          all.extend(names(&source, walk.head)?);
          all.sort_unstable();
          all.dedup();
          for name in all.iter().rev() {
            walk.stack.push((child(&source, name), child(&dest, name)));
          }
        }
      }
      (Some(old), None) => {
        let directory = old.1 == NodeKind::Directory;
        walk.plan.restores.push((
          to,
          Restore::Set {
            brought: None,
            directory,
            text: old.2,
            props: old.3,
          },
        ));
        if directory {
          for name in names(&source, walk.revision)?.iter().rev() {
            walk.stack.push((child(&source, name), child(&dest, name)));
          }
        }
      }
      (None, Some(new)) => {
        let directory = new.1 == NodeKind::Directory;
        walk.plan.restores.push((to, Restore::Remove { directory }));
        if directory {
          for name in names(&source, walk.head)?.iter().rev() {
            walk.stack.push((child(&source, name), child(&dest, name)));
          }
        }
      }
    }
    if slates_rt::futures::now_ns() >= deadline_ns {
      return Ok(walk.stack.is_empty());
    }
  }
}
