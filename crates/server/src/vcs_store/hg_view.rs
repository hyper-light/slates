//! A Mercurial repository's served view, lent from A-662's map on its owner shard (A-680 part 2; §4.17.3 "Pull and
//! clone" items 1 and 2, "Access" last item and piece 4): the [`HgStores`] both edges reach a repository's store
//! through (the HTTP site, `crate::vcs_http::hg`; the SSH site's `hg serve --stdio`, A-679), made afresh for each step
//! from what the shard holds then, so nothing of the shard is held across a wait, as git's sites are lent theirs.
//!
//! **What a view is made of.** The repository's index and the shard's content store, from its store in the map
//! ([`super::VcsStores::mercurial_ref`]); and its register's state at that instant: its phase roots (the phases log,
//! A-642), its bookmarks (each `bookmark:<label>:` name of its namespace with the node its green's head names, A-657)
//! and whether it publishes. The view is `slates_hg::serve::indexed::Indexed` (A-680 part 1): what 7.2's `served`
//! filter serves of them. The register is the record: an imported repository's phase roots and bookmarks are served
//! once its import records them there (A-687), and every changeset is public until then.
//!
//! **Publishing.** A repository's record carries no `phases.publish` yet, so every repository publishes, as a
//! Mercurial repository does by default (§4.17.3 decision 3).
//!
//! **Not lent.** A repository whose store the map does not hold here; one an import is filling, whose index is put in
//! place when the import ends (A-675), so no client is served a history that is half there; and one whose view cannot
//! be made: a marker or a changelog row the store cannot read, or a view past the request's room, each counted
//! ([`UNMADE`]). The site answers each `503`.
//!
//! **Bounds.** The register's roots and bookmarks a view reads are copied, each list charged to the request's room
//! before it is made, as the view charges its own state there.
//!
//! **Pushes** (A-680 part 2b). The lender lands a push too ([`HgStores::push`]): on the same terms (not while an import
//! fills the store, an unmade view counted), the shard's state is handed to `crate::repository::hg_push::unbundle`,
//! which judges the push over the view [`with_view`] makes and lands it into the repository's store in the map, both in
//! one step, so nothing moves between the view the push read and its entry.

use core::mem::size_of;

use slates_db::catalog::RepositoryId;
use slates_hg::index::HgIndex;
use slates_hg::phases::{Phase, PhaseHead};
use slates_hg::push::Outcome;
use slates_hg::push::held::IndexHeld;
use slates_hg::serve::indexed::{Indexed, State};
use slates_hg::{Admission, Node, PastAdmission};

use slates_vcs::ContentStore;

use crate::repository::hg_push::{self, BOOKMARK_TAG, Pushing, bookmark_of};
use crate::state::ShardState;
use crate::vcs_http::hg::HgStores;

/// Counter: a Mercurial repository whose view could not be made (a marker or changelog row its store cannot read, or a
/// view past the request's room), answered as not served. Format: a counter name in the status report.
pub(crate) const UNMADE: &str = "vcs_store.hg_view.unmade";

/// Decided (module docs, "Publishing"): whether a repository publishes, until its record carries 7.2's `phases.publish`;
/// a push lands on the same terms (`hg_push::unbundle`).
pub(crate) const PUBLISHING: bool = true;

/// The daemon's lender (module docs): the served views of the Mercurial repositories this shard's map holds.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ServedViews;

impl HgStores for ServedViews {
  type Store<'s> = Indexed<'s>;

  fn holds(&self, id: RepositoryId) -> bool {
    crate::state::with_state(|state| held(state, id)).unwrap_or(false)
  }

  fn lend<R>(
    &mut self,
    id: RepositoryId,
    room: &mut Admission,
    step: impl for<'s> FnOnce(&Self::Store<'s>, &mut Admission) -> R,
  ) -> Option<R> {
    crate::state::with_state(|state| lend_in(state, id, room, step)).flatten()
  }

  async fn push(
    &mut self,
    id: RepositoryId,
    pushing: Pushing<'_>,
    room: &mut Admission,
  ) -> Option<Outcome> {
    if !crate::state::with_state(|state| held(state, id)).unwrap_or(false) {
      return None;
    }
    let outcome = hg_push::unbundle(id, pushing, room).await;
    if outcome.is_none() {
      crate::state::with_state(|state| {
        crate::fleet::count_refusal_in(state, UNMADE);
      })
      .unwrap_or_default();
    }
    outcome
  }

  fn push_room(&self, id: RepositoryId) -> Option<u64> {
    crate::state::with_state(|state| crate::vcs_share::room(state, id).bytes)
  }
}

/// Whether repository `id`'s Mercurial store is held here with no import filling it.
fn held(state: &ShardState, id: RepositoryId) -> bool {
  state.vcs_stores.mercurial_ref(id).is_some() && !state.vcs_stores.importing(id)
}

/// Runs `step` over repository `id`'s view, made within `room` from what `state` holds now; none when it is not lent
/// (module docs), an unmade view counted.
fn lend_in<R>(
  state: &mut ShardState,
  id: RepositoryId,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, &mut Admission) -> R,
) -> Option<R> {
  if !held(state, id) {
    return None;
  }
  let made = with_view(state, id, room, |view, _, room| step(view, room));
  if made.is_none() {
    crate::fleet::count_refusal_in(state, UNMADE);
  }
  made
}

/// Runs `step` over repository `id`'s served view, made within `room` from what `shard` holds now, and what its index
/// holds as a push verifies against it (`IndexHeld`); none when its store is not in the map or the view cannot be made
/// (module docs, "Not lent"), which the caller counts.
pub(crate) fn with_view<R>(
  shard: &ShardState,
  id: RepositoryId,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, IndexHeld<'s>, &mut Admission) -> R,
) -> Option<R> {
  let (content, store) = shard.vcs_stores.mercurial_ref(id)?;
  let (roots, bookmarks) = register_of(shard, id, room).ok()?;
  let read = State {
    roots: &roots,
    bookmarks: &bookmarks,
    publishing: PUBLISHING,
  };
  let view = Indexed::new(&store.index, content, read, room).ok()?;
  Some(step(&view, IndexHeld::new(&store.index, content), room))
}

/// [`with_view`] with the texts read through `over` where it is given (A-720: a push's fork, which holds the
/// repository's revisions and the push's), the served view still the store's index as it is; `step` is handed the
/// content store too.
pub(crate) fn with_view_over<R>(
  shard: &ShardState,
  id: RepositoryId,
  over: Option<&HgIndex>,
  room: &mut Admission,
  step: impl for<'s> FnOnce(&Indexed<'s>, IndexHeld<'s>, &'s ContentStore, &mut Admission) -> R,
) -> Option<R> {
  let (content, store) = shard.vcs_stores.mercurial_ref(id)?;
  let (roots, bookmarks) = register_of(shard, id, room).ok()?;
  let read = State {
    roots: &roots,
    bookmarks: &bookmarks,
    publishing: PUBLISHING,
  };
  let view = Indexed::new(&store.index, content, read, room).ok()?;
  let held = IndexHeld::new(over.unwrap_or(&store.index), content);
  Some(step(&view, held, content, room))
}

/// `count` values of `T` charged to `room`, before they are made.
fn charge_each<T>(room: &mut Admission, count: usize) -> Result<(), PastAdmission> {
  let each = u64::try_from(size_of::<T>()).unwrap_or(u64::MAX);
  let count = u64::try_from(count).unwrap_or(u64::MAX);
  room.charge(each.saturating_mul(count))
}

/// Bookmarks: each its name and the node it names.
type BookmarkHeads = Vec<(Vec<u8>, Node)>;

/// The phase roots and bookmarks repository `id`'s register holds now, copied and charged to `room` (module docs): a
/// root whose phase or node Mercurial does not have, a name under the bookmarks' tag that no bookmark's name is, and a
/// bookmark whose green names no head are not read, since the register holds none of them for a push it took.
fn register_of(
  state: &ShardState,
  id: RepositoryId,
  room: &mut Admission,
) -> Result<(Vec<PhaseHead>, BookmarkHeads), PastAdmission> {
  let repositories = state.db.partition().repositories();
  charge_each::<PhaseHead>(room, repositories.phase_roots(id).count())?;
  let roots = repositories
    .phase_roots(id)
    .filter_map(|(phase, node)| {
      Some(PhaseHead {
        phase: Phase::from_number(i32::from(phase))?,
        node: Node::try_from(node).ok()?,
      })
    })
    .collect();
  charge_each::<(Vec<u8>, Node)>(room, repositories.branches(id, BOOKMARK_TAG).count())?;
  let mut bookmarks = Vec::new();
  for (name, green) in repositories.branches(id, BOOKMARK_TAG) {
    let head = super::hg_made::head_of(state, green);
    let (Some(bookmark), Some(head)) = (bookmark_of(name), head) else {
      continue;
    };
    room.charge(u64::try_from(bookmark.len()).unwrap_or(u64::MAX))?;
    bookmarks.push((bookmark, head));
  }
  Ok((roots, bookmarks))
}

#[cfg(test)]
#[path = "hg_view/tests.rs"]
mod tests;
